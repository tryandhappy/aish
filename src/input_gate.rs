use crate::ui;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Passthrough 入力リクエストの再発行が有効になる PTY 静音時間。
const PASSTHROUGH_QUIET: Duration = Duration::from_millis(50);

/// パススルー入力スレッド再開の不変条件を 1 箇所に閉じ込める型。
///
/// 「入力スレッドが idle に戻ったら、PTY 出力が 50ms 静音した時点で Passthrough
/// リクエストを 1 回だけ再発行する」を idle / pending / 静音タイマの 3 状態で管理する。
/// 3 つを個別の変数で持ち回ると、どれかの再設定を忘れて「入力リクエストが再送されず
/// ハングする」(CLAUDE.md 実装上の注意) を再発させるため、変更は必ずこの型経由で行う。
pub struct InputGate {
    /// 入力スレッドがリクエスト受付可能か
    idle: bool,
    /// 次の静音時に Passthrough を発行すべきか
    pending: bool,
    /// 静音判定の基準時刻 (最後に PTY 出力を観測した時刻)
    last_pty_output: Instant,
    prompt_tx: mpsc::Sender<ui::InputRequest>,
}

impl InputGate {
    /// 起動直後は「idle かつ発行待ち」(最初の main loop tick で静音判定が走る)。
    pub fn new(prompt_tx: mpsc::Sender<ui::InputRequest>) -> Self {
        Self {
            idle: true,
            pending: true,
            last_pty_output: Instant::now(),
            prompt_tx,
        }
    }

    /// 入力スレッドが idle に戻った。次に PTY 出力が静まったら Passthrough を再発行する。
    ///
    /// private: 直接呼ばず、`rearm_on_drop()` guard を「idle に戻る arm」の入口で
    /// 取得すること。旧実装はこの呼び出しを全 exit point (continue / break / `?`) に
    /// 手書きしており、呼び忘れ = 入力ハングを 2 回起こした。guard ならスコープ離脱の
    /// 経路を問わず必ず発火する。
    fn arm_passthrough(&mut self) {
        self.idle = true;
        self.pending = true;
        self.last_pty_output = Instant::now();
    }

    /// Drop 時に必ず `arm_passthrough()` する RAII ガードを返す。
    ///
    /// 「入力スレッドが idle に戻る arm」(AiPrompt / Line / PassthroughEnded) の入口で
    /// `let _rearm = gate.rearm_on_drop();` と取得する。arm の処理がどの経路で終わっても
    /// (正常完了 / continue / break / `?` エラー伝播) guard の Drop で再 arm されるため、
    /// 出口ごとの呼び忘れが構造的に起きない。PtyData arm (入力スレッド継続中) では
    /// 取得しないこと。guard 生存中は InputGate を他から触れない (借用で保証)。
    pub fn rearm_on_drop(&mut self) -> RearmOnDrop<'_> {
        RearmOnDrop(self)
    }

    /// PTY 出力を観測した (静音タイマをリセット)。
    pub fn note_pty_output(&mut self) {
        self.last_pty_output = Instant::now();
    }

    /// 発行条件 (pending && idle && 50ms 静音) が揃っていれば Passthrough を送信し
    /// busy 状態へ遷移する。判定・送信・遷移を分離しないことで「fire したのに send
    /// し忘れる」「send したのにフラグを倒し忘れる」を構造的に排除する。
    /// send 失敗 (受信側 drop = 終了間際) でも遷移は行う (旧コード互換の `let _`)。
    pub fn maybe_request_passthrough(&mut self) {
        if self.pending && self.idle && self.last_pty_output.elapsed() > PASSTHROUGH_QUIET {
            let _ = self
                .prompt_tx
                .send(ui::InputRequest::Passthrough(String::new()));
            self.pending = false;
            self.idle = false;
        }
    }
}

/// `InputGate::rearm_on_drop` が返す RAII ガード。Drop で必ず再 arm する。
pub struct RearmOnDrop<'a>(&'a mut InputGate);

impl Drop for RearmOnDrop<'_> {
    fn drop(&mut self) {
        self.0.arm_passthrough();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate_with_rx() -> (InputGate, mpsc::Receiver<ui::InputRequest>) {
        let (tx, rx) = mpsc::channel();
        (InputGate::new(tx), rx)
    }

    /// 静音時間を経過済みに偽装する (実 sleep を避ける)。
    fn force_quiet(gate: &mut InputGate) {
        gate.last_pty_output = Instant::now() - Duration::from_millis(60);
    }

    #[test]
    fn fires_once_after_quiet_period() {
        let (mut gate, rx) = gate_with_rx();
        force_quiet(&mut gate);
        gate.maybe_request_passthrough();
        assert!(matches!(
            rx.try_recv(),
            Ok(ui::InputRequest::Passthrough(s)) if s.is_empty()
        ));
        // 2 回目は pending/idle が倒れているので発行されない
        force_quiet(&mut gate);
        gate.maybe_request_passthrough();
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn does_not_fire_before_quiet_period() {
        let (mut gate, rx) = gate_with_rx();
        gate.note_pty_output();
        gate.maybe_request_passthrough();
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn rearm_allows_next_fire() {
        let (mut gate, rx) = gate_with_rx();
        force_quiet(&mut gate);
        gate.maybe_request_passthrough();
        let _ = rx.try_recv();
        gate.arm_passthrough();
        force_quiet(&mut gate);
        gate.maybe_request_passthrough();
        assert!(matches!(
            rx.try_recv(),
            Ok(ui::InputRequest::Passthrough(_))
        ));
    }

    #[test]
    fn guard_rearms_on_scope_exit() {
        let (mut gate, rx) = gate_with_rx();
        force_quiet(&mut gate);
        gate.maybe_request_passthrough();
        let _ = rx.try_recv();
        // arm を直接呼ばず guard のスコープ離脱だけで再 arm されること
        {
            let _rearm = gate.rearm_on_drop();
        }
        force_quiet(&mut gate);
        gate.maybe_request_passthrough();
        assert!(matches!(
            rx.try_recv(),
            Ok(ui::InputRequest::Passthrough(_))
        ));
    }

    #[test]
    fn transitions_even_if_receiver_dropped() {
        let (mut gate, rx) = gate_with_rx();
        drop(rx);
        force_quiet(&mut gate);
        gate.maybe_request_passthrough(); // panic しない
        assert!(!gate.pending);
        assert!(!gate.idle);
    }
}

#!/bin/bash

cargo build --release && sudo install -m 755 target/release/aish /usr/local/bin/aish


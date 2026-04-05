# BUILD



sudo usermod -aG docker USER


cargo install cross --git https://github.com/cross-rs/cross                                                                                                             
                                                                                                                                                                          
cross build --release --target x86_64-pc-windows-gnu                                                                                                                    
cross build --release --target aarch64-unknown-linux-gnu                                                                                                                
cross build --release --target armv7-unknown-linux-gnueabihf                                                                                                            
cross build --release --target x86_64-unknown-linux-musl 


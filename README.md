# Sagittarius

A simple tcp forward proxy. 

Client which is based on [async-std](https://github.com/async-rs/async-std) starts a SOCKS5 server and forwards to server. 

To show the difference, Server is based on [tokio](https://github.com/tokio-rs/tokio).

There's no encryption or obscuration. It might be enough for https. 
Anyway, for learning purposes only :-)
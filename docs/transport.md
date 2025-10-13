# TCP/TLS 传输配置

`proxy-transport` 使用 rustls 和 tokio-rustls，为控制连接（service_port）和数据连接（bridge_port）提供统一的异步字节流接口。两端通过配置选择 TCP 或 TLS 1.3，配置必须匹配。省略 `[tls]` 或设置 `enabled = false` 时使用普通 TCP。

外部用户到服务端 client_port、客户端到本地服务的连接使用各自的业务协议。

## 启用 TLS

服务端 config.toml：

```toml
[tls]
enabled = true
cert_file = "certs/server.pem"
key_file = "certs/server-key.pem"
```

客户端 config.toml：

```toml
[tls]
enabled = true
ca_file = "certs/ca.pem"
server_name = "proxy.example.com"
```

路径相对于进程工作目录。证书、信任根和私钥使用 PEM 格式；服务端证书文件按叶证书、中间证书的顺序包含证书链。私钥支持 PKCS#1、PKCS#8 和 SEC1。

客户端仍连接 server_ip；server_name 指定证书中预期的 DNS 名称或 IP，必须匹配证书的 subjectAltName。ca_file 是客户端信任的 CA 证书集合。启动时加载配置和证书，配置错误直接报错退出。

TLS 握手限时 10 秒。握手失败时关闭连接；TLS 模式严格校验证书信任链、有效期和服务器名称。服务端提供证书，客户端通过证书验证服务器身份。客户端身份认证属于后续功能，当前服务端接受完成 TLS 握手的客户端。

## 本地试用证书

使用 OpenSSL 在临时目录生成供测试使用的自签名证书：

```sh
mkdir -p /tmp/nat-proxy-certs
openssl req -x509 -newkey rsa:2048 -sha256 -nodes -days 7 \
  -keyout /tmp/nat-proxy-certs/server-key.pem \
  -out /tmp/nat-proxy-certs/server.pem \
  -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost" \
  -addext "basicConstraints=critical,CA:FALSE"
```

服务端 cert_file 和 key_file 使用上述绝对路径。客户端 ca_file 指向该 server.pem，server_name 设置为 localhost。客户端 server_ip 设置为实际连接的服务端 IP。证书文件通过可信渠道分发给客户端，私钥仅保存在服务端。

## 代码接口

客户端启动时创建一次 Connector，主循环调用：

```rust
let connector = proxy_transport::Connector::new(&cfg.tls)?;
let service = connector.connect(service_addr).await?;
let (service_rx, service_wx) = tokio::io::split(service);
```

服务端启动时创建一次 Acceptor，在 TCP accept 后调用：

```rust
let acceptor = proxy_transport::Acceptor::new(&cfg.tls)?;
let (tcp, peer_addr) = listener.accept().await?;
let stream = acceptor.accept(tcp).await?;
```

返回的流支持 AsyncRead、AsyncWrite，以及 tokio::io::copy_bidirectional。控制消息和心跳写入后调用 flush，将 TLS 缓冲数据发到网络。服务端仍通过来源 IP 关联控制连接与数据连接。

库参考：[tokio-rustls](https://docs.rs/tokio-rustls/latest/tokio_rustls/)。

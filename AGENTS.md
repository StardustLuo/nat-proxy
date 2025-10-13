# nat-proxy

Rust/Tokio 实现的 TCP 反向代理，包含服务端、客户端和共享传输层。

## 文档目录

- [TCP/TLS 传输配置](docs/transport.md)

## 开发约定

- 每个函数、方法提供文档注释。
- 文档描述当前行为和实现。
- 使用 `cargo test --workspace` 验证工作区。

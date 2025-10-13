# nat-proxy 问题清单

## 1. 文档范围

本文档记录对当前源码进行静态审查时发现的通信正确性缺陷、安全风险、恢复问题、部署前提和测试缺口。它描述的是当前代码行为，不代表相关功能已经修复。

当前通信模型包含：

- `proxy-client -> proxy-server:service_port`：长期控制连接，承载心跳和建桥请求。
- `proxy-client -> proxy-server:bridge_port`：每个公网会话使用一条独立的数据 bridge。
- `proxy-client -> [::1]:local_service_port`：client 连接本地真实服务。
- `public client -> proxy-server:client_port`：公网用户连接 server 暴露的端口。

## 2. 问题摘要

| ID | 严重度 | 问题 |
|---|---|---|
| BUG-001 | 严重 | 控制连接和 bridge 缺少身份认证与加密 |
| BUG-002 | 严重 | 超时的旧 bridge 可能被错误分配给新的公网连接 |
| BUG-003 | 严重 | 公网端口绑定失败会留下永久的孤儿控制连接 |
| BUG-004 | 高 | 所有新连接的建桥阶段完全串行 |
| BUG-005 | 中 | `::fefe:` 源地址绑定依赖仓库中未声明的系统配置 |
| BUG-006 | 中 | 畸形控制帧可以使 `proxy-client` panic |
| BUG-007 | 中 | 一次 bridge 连接失败会拆除整条控制连接 |
| BUG-008 | 中 | bridge 建立后没有本地服务就绪确认 |
| BUG-009 | 低 | server 忽略数据转发错误 |
| ENG-001 | 工程风险 | 配置文件依赖进程工作目录，多处解析失败直接 panic |
| LIMIT-001 | 设计边界 | 公网侧只支持 IPv4，本地服务地址固定为 IPv6 loopback |
| TEST-001 | 工程风险 | 没有协议、超时、并发或断线恢复测试 |

## 3. 详细问题

### BUG-001：控制连接和 bridge 缺少身份认证与加密

**位置**

- `server/src/main.rs:28-50`：server 直接接受 `service_port` 上的第一条连接。
- `server/src/main.rs:156-165`：bridge 只校验来源 IP 是否与控制连接来源 IP 相同。

**触发条件**

- 未受信任的连接能够访问 `service_port` 或 `bridge_port`。
- 或者 bridge 连接端与其他主机共享同一个 NAT 公网 IP。

**影响**

- 其他连接可以抢先占用控制会话，使合法 client 无法接入。
- 同一 NAT 公网 IP 后的其他主机可能抢走 bridge。
- 不同 IP 的连接虽然会校验失败，但会消耗当前唯一一次 `accept()`，使合法 bridge 失败。
- 控制元数据和数据面流量均为明文。

**建议**

- 对控制连接进行双向身份认证，并使用 TLS 或等价的安全传输。
- 为每个建桥请求生成不可预测的一次性 token，bridge 建立后必须先完成握手校验。
- 不要把来源 IP 相同当作身份认证。

### BUG-002：超时的旧 bridge 可能被错配给新连接

**位置**

- `server/src/main.rs:140-173`：server 为每个请求重新监听同一个 `bridge_port`，并只等待 10 秒。
- `client/src/main.rs:91-106`：client 的 `TcpStream::connect()` 没有对应的 10 秒超时。

**触发条件**

1. server 收到公网连接 A，发送建桥请求 A。
2. client 发起 bridge A 的 TCP `connect()`，但因为丢包或网络故障持续重传超过 10 秒。
3. server 超时，关闭 A 的 listener 和公网连接，但 client 的 `connect()` 仍未结束。
4. server 收到公网连接 B，在同一端口上创建新 listener，并发送请求 B。
5. bridge A 的旧 TCP 重传此时连入新 listener。
6. server 没有 request ID 可供校验，将 bridge A 当成 bridge B。

该问题不需要攻击者，仅需要旧 `connect()` 的生存时间超过 server 的请求状态。

**影响**

- 公网连接 B 的数据被送入 client 为 A 建立的本地服务连接。
- 本地服务看到的来源地址是 A，而实际数据来自 B。
- 后续请求还可能继续发生错位或触发控制连接重置。

**建议**

- 给每个 `ConnectRequest` 分配唯一 `request_id`。
- bridge TCP 建立后首先发送 `request_id` 和认证信息，校验成功后才开始转发。
- client 和 server 对建桥使用一致的截止时间；但只增加 client 超时不能替代 request ID。

### BUG-003：公网端口绑定失败会留下孤儿控制连接

**位置**

- `server/src/main.rs:51-112`：控制连接被拆分后，心跳收发 task 先被启动。
- `server/src/main.rs:114-120`：之后才绑定 `client_port`，失败后直接 `continue`。

**触发过程**

1. client 成功建立控制连接。
2. server 启动该连接的心跳发送和接收 task。
3. `client_port` 因端口被占用等原因绑定失败。
4. server 主流程通过 `continue` 返回外层循环，重新等待一条新控制连接。
5. 旧心跳 task 仍持有 TCP 读写半部和 `Arc`/watch channel，因此旧连接不会关闭。
6. client 持续收到心跳，认为连接健康，不会重连。

**影响**

- server 主流程等待 client 重连，client 则因旧心跳正常而永远不重连。
- 旧控制连接只会收发心跳，不再可能收到建桥请求。
- 即使端口占用后来解除，server 也不会自动重试绑定 `client_port`。

**建议**

- `client_port` 绑定失败时，在返回外层循环前显式发送 abort 并等待心跳 task 退出。
- 使用结构化并发管理整个控制会话，任一必要步骤失败时统一取消会话所有 task。
- 也可以先成功绑定必要 listener，再将控制会话标记为可用。

### BUG-004：建桥阶段完全串行

**位置**

- `server/src/main.rs:122-183`：`client_listener.accept()` 、发送请求和最长 10 秒的 bridge 等待都在同一个 `select!` 分支内执行。

**影响**

- server 等待当前 bridge 时不会接受下一个公网连接。
- bridge 端口不可达时，新连接处理速度最差降为每 10 秒一个。
- 高并发或故障期间容易填满 TCP backlog。
- 已经建立的数据转发 task 可以并发，问题仅在连接建立阶段。

**建议**

- 长期监听 `bridge_port`，不为每个公网连接重复创建 listener。
- 用 `request_id -> pending connection` 表管理多个并发建桥请求。
- 将每个公网连接的建桥过程放入独立 task，并设置待处理数量上限。

### BUG-005：`::fefe:` 源地址绑定依赖未声明的系统配置

**位置**

- `client/src/main.rs:16-19`：将 IPv4 转换为自定义 `::fefe:aabb:ccdd` IPv6 地址。
- `client/src/main.rs:108-122`：将自定义 IPv6 地址和原始源端口绑定到本地 socket。

**问题**

- `::fefe:` 不是标准的 IPv4-mapped IPv6 `::ffff:` 地址。
- 普通 Linux 配置通常不允许进程直接绑定未被认定为本地的 IPv6 地址。
- 代码没有设置 `IP_FREEBIND`，仓库也没有提供 IPv6 local route、sysctl 或 capability 配置说明。
- 如果原始公网客户端源端口小于 1024，非特权 proxy-client 还可能缺少绑定低位端口所需的权限。

**影响**

- `local_service_socket.bind()` 返回 `EADDRNOTAVAIL` 或 `EACCES`。
- bridge 已经连到 server，但 client 无法连接本地服务，公网连接随后被关闭。

**建议**

- 明确文档化该地址编码的目的和操作系统配置。
- 启动时主动检查所需路由、sysctl 和 capability，不要等到真实连接到达后才发现。
- 如果真实需求只是向上层服务传递来源地址，评估 PROXY protocol 等显式元数据方案是否更合适。

### BUG-006：畸形控制帧可以使 client panic

**位置**

- `client/src/main.rs:69-94`：client 以固定 8 字节读取控制帧。
- `client/src/main.rs:92`：对 `bincode::decode_from_slice()` 结果直接 `unwrap()`。

**影响**

- 任何不是心跳、且无法解码为 `ConnectRequest` 的 8 字节帧都可以终止整个 client 进程。
- 协议没有 magic、版本、消息类型和长度字段，不利于安全扩展。

**建议**

- 解码失败时记录协议错误并只关闭当前控制会话，不要 panic。
- 定义明确的帧头，至少包含协议版本、消息类型、长度和 request ID。

### BUG-007：单次 bridge 失败会拆除控制连接

**位置**

- `client/src/main.rs:96-106`：bridge `connect()` 失败后执行 `break`，离开整个控制接收循环。

**影响**

- 单个公网会话的瞬时错误会重置所有新建连接的控制面。
- client 立即重连时，server 可能还没有重新进入 `service_port` 监听；一次 `Connection refused` 会带来额外 10 秒重试延迟。
- 已经建立的数据 task 可能仍然存活，而新连接暂时无法建立。

**建议**

- 将 bridge 建立失败作为单个 request 的失败处理。
- 通过控制通道向 server 返回失败结果，然后继续处理后续请求。
- 只在控制连接本身读写失败或协议状态无法恢复时重建控制会话。

### BUG-008：没有本地服务就绪确认

**位置**

- `server/src/main.rs:156-164`：server 接受 bridge 后立即将它与公网连接配对。
- `client/src/main.rs:108-129`：client 在 bridge TCP 已经建立之后，才绑定源地址并连接本地服务。

**影响**

- 如果本地源地址绑定失败或本地服务不可达，server 不会收到明确的失败原因。
- bridge 被 client 关闭后，公网用户只看到连接突然 EOF/重置。
- server 端无法区分本地服务拒绝、源地址绑定失败和普通数据断开。

**建议**

- 在开始原始数据转发前增加 bridge 握手状态：`READY` 或带原因的 `FAILED`。
- client 只在成功连接本地服务后发送 `READY`。
- server 只在收到 `READY` 后启动公网 socket 与 bridge 的原始字节转发。

### BUG-009：server 忽略数据转发错误

**位置**

- `server/src/main.rs:162-164`：`copy_bidirectional()` 结果被 `let _ = ...` 丢弃。

**影响**

- 连接重置、读失败、写失败和异常断开不会记录。
- 运行时故障无法区分是公网侧、bridge 还是本地服务引起。

client 会以 debug 级别记录转发结果，server 两端行为不一致。

**建议**

- 记录错误类型、连接 request ID、字节计数和两侧地址。
- 区分正常 EOF 与异常 I/O 错误，避免把正常关闭记成告警。

## 4. 工程风险与设计边界

### ENG-001：配置文件依赖工作目录且错误处理过于激进

client 和 server 都使用相对路径 `config.toml`。如果从 workspace 根目录直接启动二进制，它们不会自动找到 `client/config.toml` 或 `server/config.toml`。

此外，配置读取、IP 解析和 bincode 编解码存在多处 `panic!()`/`unwrap()`。一个配置错误或协议错误可以直接结束进程，且部分 panic 信息不包含原始错误。

### LIMIT-001：地址族与本地服务地址固定

- server 的 listener 都绑定 `0.0.0.0`。
- server 会忽略非 IPv4 的公网客户端地址。
- client 的 `server_ip` 被解析为 IPv4。
- client 只会连接 `[::1]:local_service_port`，本地服务地址不可配置。

如果这些是明确的产品边界，应在 README 和配置注释中说明；如果需要 IPv6 公网接入或非 loopback 本地服务，则需要调整协议和配置结构。

### TEST-001：没有自动化通信测试

`cargo test --workspace` 能够通过，但 client 和 server 都是 `0 tests`。当前没有测试覆盖：

- ConnectRequest 与心跳的编码、分帧和异常输入。
- bridge 正常建立、建立超时和旧连接延迟到达。
- 多个公网连接并发建桥。
- `client_port` 绑定失败后的 task 取消与恢复。
- 控制连接断开、心跳超时和重连。
- 本地服务连接失败与 IPv6 源地址绑定失败。
- bridge 身份校验和请求错配。

## 5. 建议修复顺序

1. 设计有版本的控制协议，引入 `request_id`、明确消息类型和 bridge 握手。
2. 为控制连接和 bridge 增加身份认证，并保护传输内容。
3. 改为长期 bridge listener，使用 pending request 表支持并发建桥和严格配对。
4. 统一管理控制会话 task 的生命周期，任一初始化步骤失败都完整取消该会话。
5. 增加 client 就绪/失败应答，将单个 bridge 错误与控制连接故障分离。
6. 明确并自动检查 IPv6 源地址绑定所需的主机配置。
7. 补充协议单元测试和多进程端到端测试，然后再进行协议重构。

## 6. 当前验证状态

- `cargo clippy --workspace --all-targets --all-features`：通过。只发现 `== true` 的风格警告，不影响通信行为。
- `cargo test --workspace`：通过，但两个程序均为 `0 tests`。
- BUG-002 是从当前超时与连接配对逻辑确认的竞态路径，尚未在可控丢包环境中动态复现。
- BUG-005 依赖实际主机的 IPv6 路由、sysctl 和 capability。当前源码未设置或检查这些前提。

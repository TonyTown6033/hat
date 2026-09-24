# NOTES —— 这个项目是怎么运转的

一句话：**两个程序 + 一条 TCP 连接 + 一种按行文本协议。**

```text
client (你的终端界面)  <--- TCP :6969 --->  server (转发中心)
```

消息在网络上是**一行一行**的文本，每行开头是「类型」，服务器靠类型决定怎么处理。

---

## 1. 目录与构建

```text
Cargo.toml
src/
  client.rs   终端聊天客户端（TUI）
  server.rs   聊天服务器（转发 + 鉴权 + 限速）
  main.rs     （未使用/占位）
```

构建：

```console
$ cargo build
```

运行：

```console
$ cargo run --bin server      # 启动后会在标准输出打印 token
$ cargo run --bin client      # 另开一个终端
```

客户端里连接：

```text
> /connect 127.0.0.1 6969 <token>
```

---

## 2. 协议（按行，`\n` 结尾）

### 客户端 → 服务器

| 发送 | 含义 |
|------|------|
| `MSG <text>` | 聊天内容 |
| `NICK <name>` | 请求改昵称 |
| `LLM <prompt>` | 请求服务器调用配置的 LLM |

### 服务器 → 客户端

| 发送 | 含义 |
|------|------|
| `MSG <nick> <text>` | 转发聊天，`nick` 由服务器盖章 |
| `NICK <old> <new>` | 某人改名成功 |
| `YOU <nick>` | 告诉你自己的昵称 |
| `SYS <text>` | 系统消息 |
| `ERR <text>` | 出错 / 被拒绝 |

要点：

- 一条消息 = 一行，`\n` 结束（**分帧**）
- 第一个空格前是类型，后面是负载
- `MSG` 里的发送者昵称由**服务器**填写，客户端无法伪造

### 鉴权握手

```text
连接建立
  server -> "SYS authenticating\n"
  client -> "<token>\n"       （token 为 32 个十六进制字符）
  server -> "SYS Welcome to the club, buddy!\n"
  server -> "YOU <nick>\n"    （默认昵称 user#<客户端端口>）
```

## 3. 昵称规则（服务器校验）

- 非空
- 长度 ≤ 16
- 只允许字母 / 数字 / `_` / `-`
- **不区分大小写**查重（`Bob` 与 `bob` 冲突）

违反时服务器回 `ERR <原因>`，客户端用红色显示。

---

## 4. server.rs 结构

只有 4 块：

| 位置 | 作用 |
|------|------|
| `main` | 生成 token、监听 6969、每来一个连接开一个线程 |
| `client()` | **每个连接一个线程**：认证 → 按行读 → 把每行丢进 channel |
| `server()` | **唯一的主线程**：独占 `clients` 表，解析每行、广播 |
| `Client` | 一个连接的状态：socket、昵称、限速计数 |

辅助函数：

- `nickname_error()`：校验昵称
- `broadcast()`：把一行发给除自己外的所有客户端
- `authorize()`：读取并校验 token

常量：

| 常量 | 值 | 含义 |
|------|-----|------|
| `MES_FREQ` | 1 秒 | 两条消息的最小间隔（限速） |
| `BAN_FREQ` | 10 | 违规多少次封禁 |
| `BAN_LIMIT` | 10 分钟 | 封禁时长 |
| `TOKEN_LEN` | 16 字节 | token 原始长度（转成 32 个十六进制字符） |

### 关键设计：线程 + channel

```text
每个连接的线程                主线程 server()
  read 套接字                   持有 clients: HashMap<SocketAddr, Client>
  按 \n 切行                    解析 MSG / NICK
  send(NewMessage{addr,line}) -> 广播 / 改昵称 / 查重
```

- 客户端线程**不碰** `clients`，只通过 channel 汇报「某地址发来某行」
- 所有连接状态只在主线程改 → **昵称查重天然没有并发问题，无需加锁**

---

## 5. client.rs 结构

分「逻辑」和「画面」两部分。

### 逻辑

| 位置 | 作用 |
|------|------|
| `Ctx` | 所有状态：`stream`、`chat`、`user`、`server`、`started_at` |
| `Ctx::msg` / `msg_from` | 往聊天记录加一条（kind + 时间 + 作者） |
| `handle_prompt` | 以 `/` 开头就分发命令，否则发 `MSG` |
| `cmd_*` + `COMMANDS` | 每个 `/命令` 一个函数，挂在静态表里 |
| `handle_server_line` | 解析服务器发来的每一行 |
| `complete_command` | Tab 补全命令名 |

命令表：

| 命令 | 作用 |
|------|------|
| `/connect <ip> <port> [token]` | 连接服务器 |
| `/nickname <name>` | 请求改昵称 |
| `/disconnect` | 断开 |
| `/help` | 帮助 |
| `/quit` | 退出 |
| `/llm <prompt>` | 调用服务器配置的 LLM |

### 画面

| 位置 | 作用 |
|------|------|
| `main` | 事件循环：键盘 / 网络 / 重绘 |
| `chat_window` | 画聊天区（每条：`[时间] <作者> : 内容`） |
| `status_bar` | 画状态栏（在线状态、地址、运行时长、北京时间） |
| `kind_color` | 按消息类型选颜色 |

### 消息类型与颜色

`MsgKind`：`Normal`（收到）、`User`（自己）、`System`、`Warn`、`Error`。

配色是**固定 RGB**、白底黑字风格，不依赖终端主题：

| 用途 | 颜色 |
|------|------|
| 背景 | 白 `255,255,255` |
| 服务器消息 | 深绿 |
| 自己 | 黑 |
| 系统 | 青 |
| 警告 | 暗橙 |
| 错误 | 暗红 |

---

## 6. 一条消息的完整旅程

以你输入 `hello` 为例：

```text
1. 你打字 + 回车
2. client 回车分支：先本地 echo，再 handle_prompt
3. 不以 / 开头 → 发送 "MSG hello\n"
4. server 的 client() 线程按行读 → channel 发 NewMessage
5. server() 主线程解析 kind="MSG" → 广播 "MSG alice hello\n" 给别人
6. 对方 client 的 main 读循环按行切 → handle_server_line
7. "MSG" 分支 → msg_from("alice", ...) 塞进聊天记录
8. 下一帧 chat_window 把它画出来
```

记住这 8 步，整套代码就串起来了。

---

## 7. 数据流示意图

```text
                     ┌─────────────── server 进程 ───────────────┐
   你的终端          │                                            │
  ┌────────┐  TCP    │  client线程A ──channel──┐                 │
  │ client │ <------>│  client线程B ──channel──┼─> server() 主线程 │
  └────────┘         │  client线程C ──channel──┘   持有 clients 表 │
                     │                              解析/广播      │
                     └────────────────────────────────────────────┘
```

---

## 8. 分帧（为什么重要）

TCP 是字节流，不保证「你发一次 = 对方收一次」。所以：

- 发送方每条消息末尾加 `\n`
- 接收方累积字节，出现 `\n` 就切出一条完整消息

两端都这样做（server 的 `client()`、client 的 `main`），长消息和多字节字符才不会被切坏。

---

## 9. LLM 配置

服务器端的 `/llm` 从项目根目录的 `config.toml` 读取配置：

```toml
[llm]
api_key = "your-api-key"
model = "gpt-5.6-sol"
api_url = "https://superelite.studio/v1/chat/completions"
```

配置内容可以参考 Pi 的文件：`~/.pi/agent/models.json` 中的 provider `baseUrl`、`~/.pi/agent/settings.json` 中的默认模型，以及 `~/.pi/agent/auth.json` 中的 API Key。Pi 文件只是参考来源，服务器运行时不会读取 Pi 配置。Release 包会自动包含不带真实密钥的 `config.example.toml`，复制为 `config.toml` 后再填写 API Key。

当前测试配置使用 `superelite/gpt-5.6-sol`。请求在服务器后台线程执行，回答通过 `LLM` 消息返回；配置不存在、格式错误或请求失败时客户端会收到错误消息。`config.toml` 已加入 `.gitignore`，不会提交到 Git。

## 10. 已知问题 / TODO

- **CJK 宽度**：聊天区按「字符个数」截断，中文/emoji 占两格会溢出换行
- **鉴权没完全并入行协议**：服务器 `authorize()` 仍按固定 32 字节读，靠客户端多发一个 `\n` 混过去
- **系统消息作者**：`SYS`/`ERR` 仍显示成本地用户名，未用 `sys`/`err` 标签
- **无 JOIN / LEAVE 广播**：别人加入/离开不会通知
- **终端恢复**：出错提前 `return` 时可能不执行 `disable_raw_mode()`（没有 RAII guard）
- **token 只打印**：没有写入文件，客户端要靠手动复制
- **服务器用线程而非事件循环**：连接多时线程数会膨胀

---

## 11. 建议的阅读顺序

想重新读懂代码，按这个顺序读：

1. `server.rs` 的 `Client` 和 `Messages`（先搞清数据结构）
2. `server.rs` 的 `client()`（一个连接怎么读）
3. `server.rs` 的 `server()`（主线程怎么解析/广播）
4. `client.rs` 的 `Ctx`（状态）
5. `client.rs` 的 `main` 里的事件循环
6. `client.rs` 的 `handle_prompt` / `handle_server_line`（收发）
7. 最后看 `chat_window` / `status_bar`（画面）

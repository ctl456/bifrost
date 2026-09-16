# Bifrost 使用说明

这是运维手册：怎么构建、配置文件里放什么、怎么跑起来、怎么把客户端接进来，以及拿到预期之外的答案时先看哪里。设计为什么是现在这个样子见 `README.md`，各 crate 之间的契约见 `docs/architecture.md`。英文版是 `docs/usage.md`；两份内容一致，先改英文那份再改这里。

## 这是什么

Bifrost 是**放在一个 Command Code 账号前面的反向代理**。说 OpenAI Chat Completions、Anthropic Messages 或 OpenAI Responses 的客户端对着 Bifrost 说话；Bifrost 替它用私有的 `cc/1.53.1` wire 协议去和 `api.commandcode.ai` 说话。

也就是 `commandcode-proxy` 做的事，用 Rust 重写了一遍：**一个账号，多个 harness**。Claude Code、Codex CLI、编辑器插件、脚本，各自继续说自己的协议，最后都记在同一个 `user_…` key 上。

## 这不是什么

- 它不创建也不共享账号。每个客户端都得用一个账号已有的 `user_…` key；套餐允许什么，账号就拿到什么。
- 它不接受官方 CLI 自己的协议。`cmdc` 说的是 `/alpha/*`，Bifrost 对外提供的是那三种公共协议。把 `cmdc` 指过来会得到 `404`，这是设计方向，不是待修的缺口。
- 它不会把多个上游账号复用到同一个端点上。

## 环境要求

- Rust，版本由 `rust-toolchain.toml` 固定。不需要别的：不要 Node，不要 Docker。
- 运行的位置能访问 `api.commandcode.ai`。
- 一个 key。`cmdc login` 会写一份到 `~/.commandcode/auth.json`；Bifrost 不读这个文件——key 是每个请求带进来的，和客户端一样。

## 构建

```sh
cargo build --release -p bifrost-edge --bin bifrost   # 产物 target/release/bifrost
```

## 第一次运行

```sh
# 1. 一份配置。每一项都可省略，这就是全部必需内容。
cat > bifrost.toml <<'TOML'
version = 1
host = "127.0.0.1"
port = 3050
api_base = "https://api.commandcode.ai"
TOML

# 2. 信它之前先检查它。不绑定端口，不写任何东西。
./target/release/bifrost --check --config bifrost.toml

# 3. 起服务。
./target/release/bifrost --config bifrost.toml
```

然后在另一个终端，用 `~/.commandcode/auth.json` 里的 key：

```sh
KEY=$(python3 -c 'import json,os;print(json.load(open(os.path.expanduser("~/.commandcode/auth.json")))["apiKey"])')

curl -sS http://127.0.0.1:3050/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"deepseek/deepseek-v4-flash","messages":[{"role":"user","content":"Say OK"}],"max_tokens":16}'
```

`GET /health` 不需要 key，回 `OK`；客户端说连不上时先看它。

## 配置

配置分层叠加，后面的盖前面的：默认值 → 文件 → 环境变量。

| 来源 | 方式 |
|---|---|
| 指定的文件 | `--config PATH`，优先级最高 |
| 约定的文件 | `$BIFROST_CONFIG`；没有设置且当前目录存在 `bifrost.toml` 时用它 |
| 环境变量 | 下面这些，在文件之后生效 |

写错的键会被拒绝而不是忽略：拼错一个键是启动失败并指出是哪个键，而不是一条悄悄没生效的设置。

环境变量名沿用 `commandcode-proxy` 已经在用的拼法，所以现有部署不需要换一套密钥：

| 变量 | 作用 |
|---|---|
| `BIFROST_CONFIG` | 读哪个文件 |
| `PORT`、`HOST` | 监听地址 |
| `CC_API_BASE` | 上游 base URL |
| `CC_DEVICE_PROJECT_DIR` | 上报给上游的工作目录 |
| `CC_FINGERPRINT_SALT` | 批量轮换派生出的设备身份 |
| `CC_USE_PROVIDER_MODELS` | `false` 关闭上游模型目录 |
| `CMD_ZDR` | `1`/`true`/`yes` 要求零数据留存路由 |
| `BIFROST_LOG_LEVEL` | `error`、`warn`、`info`、`debug` |

每一项设置及其默认值和理由都写在 `bifrost.example.toml` 里。`--print-config` 打印这个进程实际解析出来的配置（指纹盐已打码）——你改的文件只是三层里的一层，实际生效的值才是屏幕上原来没有的答案。

### key 永远不写进这个文件

key 是随请求进来的，和客户端一样，放在 `Authorization: Bearer …` 或 `x-api-key: …` 里，两个头在所有端点上都被接受。Bifrost 不读 `~/.commandcode/auth.json`，也没有任何设置项用来存 key——因为写在配置文件里的 key 会进备份、进 unit 文件、进 `ps` 输出。跑在 systemd 下时，客户端那把 key 是客户端自己的事，unit 不需要。

## 接入客户端

任何客户端，只要（a）说这三种协议之一，（b）能被指向别的 base URL，（c）能用 bearer token 或 `x-api-key` 把 key 发出来，就能接。头部里重要的是 `user_…` 这个 token，前后的前缀会被容忍。

| 客户端 | 要设置什么 |
|---|---|
| Claude Code | `ANTHROPIC_BASE_URL=http://127.0.0.1:3050`、`ANTHROPIC_API_KEY=user_…`，再把 `ANTHROPIC_MODEL` / `ANTHROPIC_DEFAULT_HAIKU_MODEL` 指向账号里有的模型；或者写一条规则，客户端自己的默认值就不用动 |
| Codex CLI | 一个 provider：`base_url = "http://127.0.0.1:3050/v1"`、`wire_api = "responses"`、`env_key = "…"` |
| Anthropic SDK | `base_url` / `ANTHROPIC_BASE_URL`，再加 `x-api-key` 或 `auth_token` |
| OpenAI SDK | `base_url = http://127.0.0.1:3050/v1` |
| 编辑器插件（Cline、Roo、Continue 等） | 选 “OpenAI 兼容”，base URL 填 `…/v1`，模型从 `/v1/models` 里挑 |

有两件事决定一个客户端到底能不能用，而不是它的协议看起来能不能用：

- **模型名。** Bifrost 原样转发客户端点名的模型。默认写 `claude-sonnet-5` 的客户端就会去要 `claude-sonnet-5`；这个模型不在套餐里，账号会回 `401 MODEL_NOT_IN_PLAN`。注意 `GET /v1/models` 是**提供方的模型清单，不是按套餐过滤过的**——它会把本套餐用不了的模型也列出来。所以要么把客户端的模型设置指向一个真能回答的模型，要么写一条规则，让客户端的默认值原样留着。
- **上游只有流式。** 不论客户端要什么，Bifrost 都会向上游要流，客户端没要流式时由它自己拼成完整响应。客户端的 `stream` 标志不会传到上游。

### 把模型名指到别处

有些客户端改不了它要什么，也有些客户端要的名字会跟着工具版本一起变。`[models.aliases]` 里的一条规则，就是把它们要的名字接到本账号可用的模型上：

```toml
[models.aliases]
"claude-" = "deepseek/deepseek-v4-flash"
"claude-sonnet-5" = "deepseek/deepseek-v4-pro"
```

- 规则是前缀匹配，比较时不分大小写，命中的规则里**最长的那条赢**：`claude-sonnet-5` 走上面第二条，`claude-opus-4-6` 走第一条。精确的名字本来就「最长」——它能匹配自己——所以不需要单独一条精确匹配的逻辑。
- 没有规则命中的名字按客户端写的那样转发，所以拼错的名字仍然是上游的 `401 MODEL_NOT_IN_PLAN`，而不是变成另一个模型的回答。这里没有通配、也没有兜底模型：规则表不是 fallback。
- 改写发生在请求编码之前，这样才能让一个回合里出现的三个名字保持一致：上游被要的是新名字、响应里回的也是这个名字、访问日志两个都记——`model=` 是实际用的，`requested_model=` 是客户端要的。
- `evidence_archive` 归档的是客户端自己的原始字节，所以一条规则哪天被怀疑，客户端真正发的名字还在档案里。
- `GET /v1/models` 不受影响：它回答的是提供方有哪些模型（比本套餐能用的多），规则也不会往里加东西。
- 空 pattern、或者没指向任何模型的规则会直接拒绝加载：一条「看起来只有一条、实际匹配所有名字」的规则，是靠读配置读不出来的坑。

### 端点

| 方法 | 路径 | 要 key？ | 返回 |
|---|---|---|---|
| `GET` | `/health`、`/` | 否 | `OK`，仅表示活着 |
| `GET` | `/status` | 否 | 计数器：uptime、turns、refused、unauthenticated、too_large、malformed、upstream_failed、timeouts、client_stalls、inflight、max_inflight |
| `GET` | `/v1/models` | 否 | 提供方的目录，带缓存；只有第一次成功拉取之前才用内置表。它不按套餐过滤：里面也有本套餐用不了的模型 |
| `POST` | `/v1/chat/completions` | 是 | OpenAI Chat Completions，流式与整包 |
| `POST` | `/v1/messages` | 是 | Anthropic Messages |
| `POST` | `/v1/responses` | 是 | OpenAI Responses |

## 运维

它说的话只去一个地方：默认每行一个 JSON 对象，`log_format = "text"` 时每行一条纯文本。每个请求都留一行——方法、路径、状态、首字节耗时，等到信息齐了还有协议、模型、流式标志和 key 指纹。被规则指到别的模型的回合两个名字都记：`model` 是实际回答的，`requested_model` 是客户端要的。**key 本身永远不进日志。**

`GET /status` 用计数器回答这个进程一直在干什么，不需要 key：这是把「没有请求进来」和「请求进来但没工作」分开的办法。

打开 `evidence_archive = true` 后，每个回合的原始字节会被保留，每次回合往 journal 追加一行。五个读取旗标使用与服务端相同的配置——在仓库自带的 unit 下，这意味着命令行上也要带 `--config /etc/bifrost/bifrost.toml`，因为 unit 就是用这个方式指名文件的：

| 命令 | 回答的问题 |
|---|---|
| `--journal` | 按顺序发生了什么（`--kind`、`--since`、`--session`、`--limit` 收窄） |
| `--verify DIGEST` | 这些字节还是当初存进去的字节吗（`--quote` 还能顺带找字符串） |
| `--audit` | 上面这些一次问完：一行计数，然后是每条发现一行 |
| `--turn DIGEST` | 交出某个回合：它的 journal 行，加上每一半的字节 |
| `--turn --session ID` | 交出整段对话，最早的回合在前 |

`--verify` 和 `--audit` 的退出码含义相同：`0` 完好，`1` 某块字节已经不再哈希到它的名字（或某个名字根本不是摘要），`3` 字节没了——这是保留策略留下的结果，和「某个声明没成立」是两回事。

保留策略是 `audit.retain_days` 与 `audit.max_total_mb`，启动时和每天各执行一次。journal 永不裁剪；仍被窗口内某个回合引用的块会留下；上限和窗口冲突时以窗口为准，并在日志里说明。

systemd 下 `deploy/bifrost.service` 以非特权用户运行它，只给一个 socket、一条出站连接和它自己的状态目录，其它哪儿都不能写。unit 跑的两条命令都用 `--config /etc/bifrost/bifrost.toml` 指名配置文件；`--check` 接在 `ExecStartPre` 上，所以配置有问题时失败的是 unit，而不是第一个请求——而且被检查的文件就是将被服务的那个文件。`deploy/bifrost.env.example` 是这个 unit 读的环境文件；它换不了配置文件，因为命令行上点名的路径优先于 `BIFROST_CONFIG`。安装步骤写在 unit 自己的头部注释里。

## 故障对照

| 现象 | 是什么 |
|---|---|
| `Missing API key` | `Authorization: Bearer` 或 `x-api-key` 里没有 `user_…` token |
| `401 MODEL_NOT_IN_PLAN` | 模型是真的，但不在此账号套餐内；错误来自上游，并按客户端自己的错误形状返回。从 `/v1/models` 里挑一个，或者写一条规则把这个名字指过去 |
| 客户端卡住然后超时 | 推理模型在出第一个 token 之前一直在想。`/v1/messages` 专门为此发注释帧心跳；不接受这种帧的客户端仍然会超时 |
| 预请求被拒 | 它永远不会让回合失败，所以任何响应里都看不到痕迹——去日志里找那条 warn。另外记住：少了 `User-Agent` 会在看路径之前就拿到 `403` |
| `--verify` 回 `3` | 保留策略删掉了这些字节；journal 那行仍然说明了当时发生了什么 |
| 日志里出现 drift 行 | 已发布的客户端跑到 `cc/1.53.1` 前面去了。跑 `tools/check-dialect-alignment.sh`；这个 dialect 不会自己跟着动 |
| 配置加载不了 | 跑 `--check`，它会指出是哪一项。未知的键是故意拒绝的 |

## 检查

```sh
cargo test --workspace                                     # 全部门禁
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
tools/check-dialect-alignment.sh                           # 词汇表 vs 已发布的客户端
tools/smoke.sh --generate                                  # 起一个构建 + 一个真实回合
```

`tools/smoke.sh` 会起一个构建、和真实服务对话，然后把服务端自己的日志读回来：每个请求留下的访问行、被拒预请求才会出现的警告、key 从未进入日志、以及停止信号是否干净退出。两个外部检查都能证明自己有能力失败——smoke 用 `--self-test`，对齐脚本用 `CC_SELFTEST=1`。

## 已验证内容

协议形状和设备指纹是和原始 JavaScript **逐字节对比**的，而不是照着读一遍：每个 crate 把用于校验的向量放在 `fixtures/` 下，办法是原样运行 `commandcode-proxy/proxy.mjs` 的对应片段。`docs/wire-alignment.md` 记录了第一次对齐检查的结果，对象是客户端 `1.54.0`：`cc/1.53.1` 这个 dialect 不需要改动。

对真实服务的端到端验证：

- 三种协议，流式与整包，含工具调用、图片、`reasoning_effort` 与提示缓存；
- 实际发到上游的字节，从一个「记录型替身上游」读回：预请求那一对、信封的键顺序、工具定义按 `input_schema` 发出、图片形如 `{"type":"image","image":"data:…","mimeType":…}`，以及请求头（`user-agent: cli`、`x-command-code-version`、`x-project-slug`、`traceparent`）；
- Claude Code `2.1.119`（4.3 万 token 的系统提示与工具定义）与 Codex CLI `0.115.0`（Responses API）从它里面跑通，两者都答出了 `OK`。

## 与原项目的有意差异

- 模型目录刷新失败时，继续用上游上次给的那份，而不是退回到编译进二进制里的表。
- 生命周期上报的 mode 固定为 `interactive`，不会报 `non-interactive`；信封里其它字段同样不可配置。
- 没有 metrics 端点，也没有任何「打开它什么都不会发生」的开关。

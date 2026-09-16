# Bifrost 使用说明

操作手册：怎么构建、配置里写什么、怎么跑起来、怎么接客户端、结果不对时先看哪儿。
英文版是 `docs/usage.md`，改动先落在英文版；`README.md` 是总览。

## 这是什么

Bifrost 是**挡在一个 Command Code 账号前面的反向代理**。说 OpenAI Chat Completions、
Anthropic Messages 或 OpenAI Responses 的客户端连它；它替这个客户端用私有的
`cc/1.53.1` 线协议去跟 `api.commandcode.ai` 说话。

也就是 `commandcode-proxy` 做的事，用 Rust 重写了一遍：一个账号，多个 harness，各自
说自己那套协议，账都记在同一个 key 上。

## 这不是什么

- 不创建、也不共享账号：每个客户端都要一个账号本来就有的 `user_…` key，套餐允许什么
  就得到什么。
- 不接受 CLI 自己那套协议：官方 `cmdc` 说的是 `/alpha/*`，Bifrost 对外只提供三种公开
  协议，把 `cmdc` 指过来会得到 `404`，设计如此。
- 不把多个上游账号藏在同一个端点后面。

## 环境要求

- Rust，版本由 `rust-toolchain.toml` 钉住；或者用发好的镜像，那样连工具链都不需要
  （见「用容器跑」）。没有别的：不需要 Node。
- 运行的地方能访问 `api.commandcode.ai`。
- 一个 key。`cmdc login` 会写一份到 `~/.commandcode/auth.json`；除非 `[access]` 点名读
  它，Bifrost 不碰这个文件 —— 默认是每个请求把 key 带进来。

## 构建

```sh
cargo build --release --bin bifrost                    # 产物 target/release/bifrost
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

然后另开一个 shell，用 `~/.commandcode/auth.json` 里的 key：

```sh
KEY=$(python3 -c 'import json,os;print(json.load(open(os.path.expanduser("~/.commandcode/auth.json")))["apiKey"])')

curl -sS http://127.0.0.1:3050/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"deepseek/deepseek-v4-flash","messages":[{"role":"user","content":"Say OK"}],"max_tokens":16}'
```

`GET /health` 返回 `OK` 且不需要 key，客户端说连不上时先看它。

## 配置

配置分三层，后一层覆盖前一层：默认值 → 文件 → 环境变量。点名的文件用
`--config PATH`（优先级最高），约定位置是 `$BIFROST_CONFIG` 或 `./bifrost.toml`，
环境变量在文件之后生效。未知的键会被拒绝而不是忽略：写错一个键就是启动失败并点名
那个键，而不是一个悄悄什么都没做的设置。

环境变量名沿用 `commandcode-proxy` 已有的名字，所以现存部署不需要一套新的密钥：

| 变量 | 作用 |
|---|---|
| `BIFROST_CONFIG` | 读哪个文件 |
| `PORT`、`HOST` | 监听地址 |
| `CC_API_BASE` | 上游 base URL |
| `CC_DEVICE_PROJECT_DIR` | 上报给上游的工作目录 |
| `CC_FINGERPRINT_SALT` | 批量轮换推导出来的设备身份 |
| `CC_USE_PROVIDER_MODELS` | `false` 关掉上游模型目录 |
| `CMD_ZDR` | `1`/`true`/`yes` 要求只走零数据留存的路由 |
| `BIFROST_LOG_LEVEL` | `error`、`warn`、`info` 或 `debug` |

`bifrost.example.toml` 写全了每个设置、默认值和它存在的理由。`--print-config` 打印
进程真正解析出来的配置（salt 打码）：你编辑的文件只是三层里的一层。

### key 随请求进来 —— 除非这个部署自己发牌

默认情况下 key 随每个请求进来，放在 `Authorization: Bearer …` 或 `x-api-key: …`，
每个端点两种都收。Bifrost 不读 `~/.commandcode/auth.json`，也没有“配置里写个 key”
这种设置：配置里的 key 就是备份里、unit 文件里和 `ps` 输出里的 key。

`[access]` 是另一种安排，给“不止你自己一台机器”的部署用：key 从这个进程自己拥有的
文件里读，每个调用方拿到一把 token。

```toml
[access]
enabled = true
key_file = "/home/you/.commandcode/auth.json"
tokens_file = "var/tokens.json"
```

### 发一把 token

```sh
./target/release/bifrost --token-new laptop --rpm 60 --concurrency 2 --config bifrost.toml
# token `laptop` issued: bfr_9f0c…   —— 只在这里显示这一次
./target/release/bifrost --token-list           --config bifrost.toml
./target/release/bifrost --token-revoke phone   --config bifrost.toml
```

- token 就是客户端手里的凭据，放在原来放 key 的那个头里。客户端其余配置一个字都不用
  改。文件里存的是它的 sha256，所以这个文件可以随便读、备份、复制 —— 它本身不是凭据；
  丢了就重发（`--token-new` 只打印一次，权限 `0600`）。
- 名字用过的就一直占着：撤销是打标记而不是删除，所以日志里见到的名字和从来没发过的
  名字能区分开。
- `--rpm` 和 `--concurrency` 按 token 生效，默认不限，防止一个调用方把这个部署的位置
  占满。服务进程在文件变化时重新读它，所以发牌和撤销都能作用到正在跑的部署；文件被
  删掉等于没有任何 token，也就是全部拒绝。
- 这里 key 不是 token：`user_…` key 发给发牌的部署会被 `401` 拒绝 —— 一个仍然有效的
  凭据，就是撤销够不到的凭据。
- `GET /status` 每个 token 多一行（名字、已服务、在飞、空闲、是否撤销，以及这个调用方
  的轮次在 provider 缓存上做了什么），这是聚合计数答不了的问题：谁把位置占满了，以及
  是谁把大家的 prompt 缓存搞坏了。也正因为这一行，这个页面要 token 才能看，否则 `401`；
  转发 key 的部署里没有名字，仍然匿名可读。两种形态下 `/health` 都不需要凭据。
- **key 仍然不在这个文件里。** `access.key_file` 只是点名一个本进程读的文件；key 不会
  被 `--print-config` 打印，不会进日志，也不会发给客户端。

## 端点

| 端点 | 是什么 |
|---|---|
| `POST /v1/chat/completions` | OpenAI Chat Completions |
| `POST /v1/messages` | Anthropic Messages |
| `POST /v1/responses` | OpenAI Responses |
| `GET /v1/models` | 上游自己的模型目录 |
| `GET /status` | 本进程自己的计数 |
| `GET /health`、`GET /` | 存活检查，不需要凭据 |

## 接入客户端

只要客户端（a）说这三种协议之一，（b）能把 base URL 指到别处，（c）能用 bearer token
或 `x-api-key` 带 key，就能用。头里真正起作用的是那个 `user_…` token，前后的装饰性
文字是被容忍的。

| 客户端 | 需要设置什么 |
|---|---|
| Claude Code | `ANTHROPIC_BASE_URL=http://127.0.0.1:3050`、`ANTHROPIC_API_KEY=user_…`，并且把 `ANTHROPIC_MODEL` / `ANTHROPIC_DEFAULT_HAIKU_MODEL` 指到账号真的有的模型 —— 或者写一条规则，那样客户端自己的默认值不用动 |
| Codex CLI | 一个 provider：`base_url = "http://127.0.0.1:3050/v1"`、`wire_api = "responses"`、`env_key = "…"` |
| Anthropic SDK | `base_url` / `ANTHROPIC_BASE_URL` 加上 `x-api-key` 或 `auth_token` |
| OpenAI SDK | `base_url = http://127.0.0.1:3050/v1` |
| 编辑器插件（Cline、Roo、Continue …） | 选 “OpenAI 兼容” provider，base URL 填 `…/v1`，模型从 `/v1/models` 里挑 |

有两个因素决定一个客户端能不能真跑起来：

- **模型名。** Bifrost 原样转发客户端点名的模型，所以默认写死 `claude-sonnet-5` 的
  客户端会得到 `401 MODEL_NOT_IN_PLAN`。`GET /v1/models` 是上游的清单而不是按套餐过滤
  过的，它会列出本套餐拒绝的模型 —— 要么把客户端的模型设置指到能答的，要么写一条规则。
- **上游只支持流式。** 不管客户端要不要流，Bifrost 都向上游要流，客户端没要流时自己
  把整个 body 拼出来。客户端 `stream` 这个标志不会传到上游。

### 把模型名指到别处

有些客户端改不了它要什么，或者它要的名字每个版本都在变。`[models.aliases]` 里的一条
规则把客户端要的名字映射到本账号能用的名字上：

```toml
[models.aliases]
"claude-" = "deepseek/deepseek-v4-flash"
"claude-sonnet-5" = "deepseek/deepseek-v4-pro"
```

- 前缀匹配、忽略大小写，最长的匹配胜出，所以精确名字不需要特例。
- 没有规则匹配的名字原样转发。没有兜底，也没有默认模型：规则表不是 fallback，拼错的
  名字仍然是上游真实的 `401`。
- 重写发生在编码之前，几个名字因此始终一致：上游被要的是新名字，响应报的也是它，
  access 行里两者都记 —— 回答的是 `model=`，被要的是 `requested_model=`。

## 运维

每个请求都留一行，包括根本没走到端点的那些：方法、路径、状态、到开始响应花的时间，
以及这一轮走到那一步之后知道的协议、模型、是否流式、session，和被计费那个 key 的
指纹。被规则改过模型的轮次两个名字都记；key 本身不会出现。

`GET /status` 是同一个问题用计数回答（跑了多久、答了多少轮、拒了多少、上游失败多少
次），运维靠它区分“没东西进来”和“有东西但没 work”。里面不引用 key、模型或 body。

它还带一个 `cache`，是本进程服务过的所有轮次的合计：`prompt_tokens`、provider 从缓存
里直接读的 `cached_tokens`、写进缓存的 `cache_write_tokens`，以及 `completion_tokens`。
`cache_hit_rate` 是 `cached_tokens / prompt_tokens`，保留四位小数；在一轮都没计过之前
是 `null` —— 没有 token 的比率是“没测过”，不是 0，按 `0.0` 分支的读者会从一个还没看过
的页面上得出错误结论。这是客户端自己算不出来的那个数：harness 只知道自己的 token 估算，
不知道 provider 的缓存对这段 prompt 做了什么。前缀一旦不再稳定，在这里表现为比率下降而
`cache_write_tokens` 上升 —— 这比账单更早告诉运维发生了什么。报的是计数而不是钱：本进程
不读任何价目表。

在 systemd 下，`deploy/bifrost.service` 用非特权用户跑它，自带 state 目录，只有一条
出网连接和一个 socket，除了那个目录无处可写。unit 里两条命令都用
`--config /etc/bifrost/bifrost.toml` 点名配置，`--check` 挂在 `ExecStartPre` 上，
所以这个构建用不了的配置是让 unit 失败而不是让第一个请求失败。安装步骤写在 unit 的
头部注释里。停止信号 SIGTERM 和 SIGINT 都认，会放完在飞的请求并留一行日志。

### 用容器跑

镜像就是同一个二进制，默认值也正好是容器要的：`0.0.0.0:3050`、每个请求转发调用方的
key、什么都不用配。entrypoint 就是那个二进制，也没有 `CMD`，所以参数列表就是这个构建
本来就有的命令行：

```sh
docker build -t bifrost .

# 一份配置，挂到命令点名的地方。
docker run --rm -v "$PWD/bifrost.toml:/etc/bifrost/bifrost.toml:ro" \
  bifrost --check --config /etc/bifrost/bifrost.toml

# 起服务：端口放出来，state 目录放在卷上。
docker run -d --name bifrost -p 3050:3050 -v bifrost-state:/var/lib/bifrost bifrost
```

`--check`、`--print-config`、`--token-new`、`--token-list`、`--token-revoke` 在容器里
跟在宿主机上是同一批命令，后三个 token 命令读的还是服务读的那份配置 —— 所以它们必须跑
在同一个卷上。容器没有 `ExecStartPre`，也不需要：进程是先校验配置再绑定端口，所以这个
构建用不了的配置是让容器失败，而不是让第一个请求失败。

`docker-compose.yml` 就是 systemd unit 在这一侧的等价物，里面每一条加固都是 unit 里的
那一条：同一个非特权账号、丢掉全部 capability 且不允许再拿、只读文件系统，外加一个可写
的命名卷 `/var/lib/bifrost` —— 这个部署自己发牌时 `var/tokens.json` 就落在那里。
`stop_grace_period` 就是它的 `TimeoutStopSec`，理由也一样：流式的一轮能跑过容器默认的
十秒。healthcheck 用的是镜像自带的那条而不是在 compose 里再写一遍，它从环境里读 `PORT`，
所以挪了端口探针不会留在原地。

### key 留在容器里的时候

`[access]` 那套在容器里也就是三行配置，其中一行由「key 能挂到哪」定死。先把一份容器读得到
的副本放好：

```sh
# 容器里的进程是 uid 10001，所以你自己 uid 下 0600 的文件它读不到。给它一份归它所有的
# 副本，而不是把原件的权限放宽 —— 原件就是凭据本身。
sudo install -d -o 10001 -g 10001 -m 0750 /srv/bifrost
sudo install -o 10001 -g 10001 -m 0400 ~/.commandcode/auth.json /srv/bifrost/auth.json
```

```toml
# 仓库根目录的 bifrost.toml
[access]
enabled = true
key_file = "/etc/bifrost/auth.json"   # 叠加文件把 key 挂到这里
tokens_file = "var/tokens.json"       # 相对路径：/var/lib/bifrost，也就是那个卷
```

```sh
BIFROST_KEY_FILE=/srv/bifrost/auth.json \
  docker compose -f docker-compose.yml -f docker-compose.access.yml up -d

# token 写进的就是服务读的那个卷，所以这是它看到的同一个文件。
docker exec bifrost /usr/local/bin/bifrost \
  --config /etc/bifrost/bifrost.toml --token-new laptop --rpm 60 --concurrency 2
```

`docker-compose.access.yml` 是叠加文件而不是第二份部署：镜像、加固、端口、state 卷都还是
基础文件那一套，它只加上要读的配置和要用的 key，两个都是 `:ro`。宿主侧路径从
`BIFROST_KEY_FILE` 读 —— 那是 `.env` 里的一行（这个文件在 .gitignore 里），而不是每条命令
都要打一遍的东西。

跟转发调用方 key 的部署相比有两处不同。`/status` 要 token 才能看，因为每个已发 token 一行
就意味着这页点了调用方的名字，这里唯一不是匿名的那页。另外带 `user_…` 来的调用方会被拒：
一个还能用的 key 就是撤销够不到的 key。

轮换账号的 key 是一次重启，因为它只在启动时读一次。发牌和撤销不是：`var/tokens.json` 是在
服务运行中重读的，所以 `--token-new` 和 `--token-revoke` 是对运行中的部署做的事。

镜像由 `.github/workflows/docker.yml` 构建 `linux/amd64` 和 `linux/arm64` 两个架构并发到
GHCR：打 `v*` tag 会发这个名字的版本并推动 `latest`，在 main 上手动跑会发 `edge`；每个
镜像还带 `sha-<commit>`，报告问题时报它 —— 那是唯一不会动的 tag。

```sh
docker run --rm ghcr.io/ctl456/bifrost:latest --help
```

## 故障对照

| 症状 | 是什么 |
|---|---|
| `Missing API key` | `Authorization: Bearer` 或 `x-api-key` 里没有 `user_…` token |
| `401 MODEL_NOT_IN_PLAN` | 模型真实存在但不在本账号套餐里；这是上游按客户端自己的错误形状回的错误。从 `/v1/models` 里挑一个，或者写条规则把这个名字指过去 |
| 客户端卡住然后超时 | 推理模型在出第一个 token 之前一直在想。`/v1/messages` 正是为此发注释帧心跳；受不了心跳的客户端照样会超时 |
| 预检被拒 | 它永远不会让一轮失败，所以任何响应里都看不到痕迹 —— 去日志里找那条 warning，并记住少了 `User-Agent` 会先吃 `403`，路径都不看 |
| 日志里出现 drift 行 | 已发布的客户端跑到 `cc/1.53.1` 前面去了。上报给上游的版本不会自己动，所以去重读那个包、重新对齐 dialect |
| 配置加载不了 | 跑 `--check`，它会点名那个设置。未知的键是故意拒绝的 |

## 检查

```sh
cargo test                                                 # 全部门禁
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
tools/smoke.sh --generate                                  # 起一个构建、跑一个真服务、打一轮真请求
```

前三条由 `.github/workflows/ci.yml` 在每次 push 和 PR 上跑。

`tools/smoke.sh` 会起一份构建去跟线上服务说话（健康、计数、模型目录、access 行），
再起一个同构建但自己发牌的部署，并把两边的日志读回来。它对发牌部署问的每个问题都用
一个不可能有效的凭据，所以这一段不花钱。`--self-test` 把上游指到一个死端口，用来自证
这个检查有能力失败。

镜像由 `.github/workflows/docker.yml` 在每次 PR 和每次 push 到 main 时构建，构建完还会
起一个容器、问它 `/health`，再把镜像自己声明的那条探针在容器里跑一遍。有这一步，
Dockerfile 才是 CI 在检查的东西，而不是只有发版时才走一遍的东西；发布也在同一个文件里，
靠 tag 触发。

## 已验证内容

协议形状和设备指纹是跟原始 JavaScript **逐字节对比**的，而不是照着读一遍：`fixtures/`
里放的就是校验向量，办法是原样运行 `commandcode-proxy/proxy.mjs` 的对应片段。

对线上服务的端到端验证：

- 三种协议，流式与整包，带工具、图片、`reasoning_effort` 和 prompt caching；
- 真正发出去的线内容，从一个会录制的假上游读回来：预检那一对请求、envelope 的键序、
  工具定义作为 `input_schema`、图片的封装，以及各种头；
- Claude Code `2.1.119`（43k token 的 system prompt 加工具定义）和 Codex CLI
  `0.115.0`（Responses API）跑通，都答了 `OK`；
- 发牌部署：token 能服务一轮而上游用部署自己的 key；`user_…` key 被拒；撤销和新发都
  能作用到正在跑的进程；额度用完答 `429`；超过并发上限答可重试的 `503`。

## 与原项目的有意差异

- 模型目录刷新失败时，继续用它上一次给的那份，而不是退回编译进去的表。
- lifecycle 的 mode 上报为 `interactive`，永远不是 `non-interactive`；envelope 里
  其他东西也都不可配置。
- 没有 metrics 端点，也没有“打开等于没打开”的开关。
- 即使客户端没要流，上游也要的是流，所以两种轮次在代理这边的记账方式是一样的。

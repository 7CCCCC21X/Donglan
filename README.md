# 规则吃单 · 独立服务（Google Cloud 东京）

把 [huice](https://github.com/7CCCCC21X/huice)（TG 合约昨收偏离提醒机器人）1.154.27 里的**规则吃单**（`rules.py`，`RULE_TAKER`）单独拿出来，做成一个只干这一件事的服务，部署在 Google Cloud 东京（`asia-northeast1`），目的是**吃单更快**。

规则、价格、份数、变量名、记录格式、所有保护措施都和 huice 一样（Railway 上的设置可以直接抄过来）；改的是“怎么跑”：

- **独立进程**：不再和原策略、预挂、复盘页挤在同一个事件循环里。huice 自己的记录里，盘口到了以后被主循环别的活儿占住的时间有 111 ms、2098 ms、2540 ms（σ 重算、复盘页重算、垃圾回收、几万条预挂记录）——这些在这里都不存在。
- **部署在东京**：huice 11-18 实测，机器人在阿姆斯特丹（`europe-west4`），到 Cloudflare 入口只要 7–31 ms，读一次盘口却要 233–311 ms：入口很近、Predict 的服务器很远。东京到它近得多，部署后用 `rt.sh latency` 量真实数字。
- **Rust 写的（3.0 起）**：同一台机器上实测，推送帧到达 → 下单请求交给网络 **0.07 ms**（预先签好）/ **0.12 ms**（当场签名）；原来的 Python 版是 0.31 ms / 0.90 ms。见“[Rust 版](#rust-版30)”。
- **路上的每一步都更短**（见下面“提速”）。
- **有自己的网页控制台和复盘页**（huice 控制台“规则吃单”一组、复盘页“规则吃单”一节的对应物）：看状态、暂停 / 恢复、切模式、在线改规则价格和份数；按规则、按日期复盘，抢单记录、盘口观察、累计盈亏曲线、导出 CSV。见“[控制台和复盘页](#控制台和复盘页)”。

> 先说清楚：这里只有“规则吃单”。原策略、预挂、提醒仍然在 Railway 上的 huice 里跑。原来的 Python 版留在 `python/` 目录当备用，一条命令就能换回去（记录、设置两边通用）。两边怎么配合见“和 Railway 上的 huice 一起用”，**一定先看**。

---

## 目录

0. [Rust 版（3.0）](#rust-版30)
1. [和 Railway 上的 huice 一起用（必看）](#和-railway-上的-huice-一起用必看)
2. [部署到 Google Cloud 东京（一步一步）](#部署到-google-cloud-东京一步一步)
3. [日常操作](#日常操作)
4. [控制台和复盘页](#控制台和复盘页)
5. [提速：具体改了什么](#提速具体改了什么)
6. [为什么是东京、怎么确认](#为什么是东京怎么确认)
7. [规则（和 huice 相同）](#规则和-huice-相同)
8. [设置一览](#设置一览)
9. [安全](#安全)
10. [记录、恢复与风控](#记录恢复与风控)
11. [和 huice 相比没搬过来的](#和-huice-相比没搬过来的)
12. [本地运行与测试](#本地运行与测试)

---

## Rust 版（3.0）

3.0 起服务是 Rust 写的（`src/`），逻辑逐行照 Python 版（2.0，`python/ruletaker/`）移植，记录格式、设置、网页控制台、Telegram 命令、`rt.sh` 的用法都不变。

**快了多少**（同一台机器，`cargo test --release bench_hot_path -- --ignored --nocapture` 和 `python python/tests/bench_hotpath.py`，记录里都已有 600 场）：

| 推送帧到达 → 下单请求交给网络 | Python 版 2.0 | Rust 版 3.0 |
|---|---|---|
| 预先签好的单 | 中位 0.31 ms | 中位 **0.07 ms** |
| 当场签名 | 中位 0.90 ms | 中位 **0.12 ms** |

说实话：这省下的是 0.2～0.8 ms；一次到 Predict 的网络往返是几毫秒到几十毫秒，**决定抢不抢得到的仍然主要是网络**（所以才部署在东京）。Rust 版的好处还有：没有垃圾回收停顿；同一个回合里从收到推送到请求字节写进网络连接，中间不会被别的任务插队；内存占用小、启动快。

**和 Python 版逐字节一致的地方**（`src/parity.rs` 用 Python 版生成的数据 `tests/vectors/` 核对）：两年里每条规则的每一场、CME 日历、市场标题核对、订单签名（普通钱包和 Predict 智能账户的 Kernel 包装）、登录签名、下单请求体的每一个字节、订单金额的取整。

**比 Python 版多做的两件事**：

- 复用连接前先看一眼服务器是不是已经把它关了（读一下套接字，不等待）：关了就换新连接，下单请求不会发到一条已断的连接上、变成“结果未知”。
- 记录写库挪到下单请求发出之后（订单日志仍然先写，崩溃恢复不受影响），请求早几十微秒出门；订单日志写不进去时（例如磁盘满）仍然先等这张单的记录写进数据库、写不进就不发，和 Python 版一样。

**换回 Python 版 / 再换回来**（记录、网页保存的设置、暂停状态两边通用，同一个数据库文件）：

```bash
RT_IMPL=python bash deploy/gcp/rt.sh update   # 换成 Python 版（之后一直用它，直到再指定）
RT_IMPL=rust bash deploy/gcp/rt.sh update     # 换回 Rust 版
```

**编译**：虚拟机上用 Ubuntu 官方源里的 Rust 工具链（`rustc-1.97` / `cargo-1.97`，和别的 Ubuntu 软件包一样有签名），依赖按 `Cargo.lock` 锁定的版本编译，cargo 会逐个核对每个包的 SHA-256。第一次部署要多 3～5 分钟编译，以后 `update` 只编译改动的部分；编译完先跑全部离线测试，有一个失败就不换、原来的版本继续运行。

---

## 和 Railway 上的 huice 一起用（必看）

1. **Railway 上把 `RULE_TAKER` 改成 `off`**，再把这边改成 `on`。两边都开 `on` 时会在同一场一起抢、一起下单（同一个账户就是买双份，也会互相把对方的单当成卖单吃掉）。顺序：这边先用 `paper` 跑起来 → Railway 改 `off` 并重新部署 → 这边改 `on`。
2. **API Key 用一把专用的**（`RULE_API_KEY`）。Predict 按 Key 限流（每把每分钟 240 次），两个进程用同一把 Key 会一起撞 429、一起停 20 秒。
   - 最好在 developers.predict.fun 再申请一把新的给这里用。
   - 如果把 Railway 上的 `RULE_API_KEY` 拿过来用，**要把 Railway 上的 `RULE_API_KEY` 删掉**：Railway 上规则吃单关掉以后，那把 Key 会被一直借给盘口和预挂（`RULE_KEY_LEND`），这边再用就会撞限流。
3. **账户：用同一个 Predict 账户，还是单独开一个？**
   - **同一个账户**（最省事，私钥和充值地址照抄）：
     - 赢的份额由 Railway 上的自动领取（`LIVE_AUTO_REDEEM=on`）一起领掉——这边不做领取。
     - Railway 的日亏损停机线（`LIVE_MAX_DAILY_LOSS_USD`）不再包含规则吃单的盈亏；这边用自己的 `RULE_MAX_DAILY_LOSS_USD` 管。
     - Railway 的持仓核对会看到“账户里有、记录里没有”的份额（规则吃单的成交）。黄金休市 4 档、黄金开盘第一档、原油开盘第一档这几场原策略和预挂本来就不做，没有影响；**黄金纽约 05:15**（以及默认关闭的 05:00、16:45、原油 00:45）原策略可能在开场后做同一场，Railway 有可能把这边的成交当成原策略某张订单的“📌 补记成交”记账——钱不会少，只是账记到了原策略名下。介意的话用单独账户，或者这边把 `RULE_GOLD_0515_CENTS` 设成 0。
     - 05:00、16:45、原油 00:45 这三条（默认关）如果在这边打开，Railway 上的预挂可能在同一场挂着另一边的买单，这边吃单时可能吃到自己的挂单（白付手续费）。
   - **单独账户**（在网站上另开一个账户、充 USDT、在网站上交易一次让授权生效）：两边互不干扰；但这边**不做自动领取**，赢的份额要定期到 predict.fun 网站上领取，否则 USDT 会越来越少。
4. **Telegram**：只发通知的话，可以直接用 Railway 那个机器人的 Token（发消息不冲突）。想在 Telegram 里用 `/status`、`/pause` 等命令，必须给这边**单独建一个机器人**（同一个 Token 只能有一个进程收消息）。

---

## 部署到 Google Cloud 东京（一步一步）

全程在浏览器里完成，不需要在自己电脑上装任何东西。

### 0. 准备

- 一个 Google 账号，打开 <https://console.cloud.google.com>。
- 第一次用要**开通结算**（绑定付款方式）：左上角菜单 → “结算”。
- 新建一个项目：顶部项目选择器 → “新建项目”，名字随意（例如 `ruletaker`），建好后选中它。

### 1. 打开 Cloud Shell

控制台右上角点 **“激活 Cloud Shell”**（`>_` 图标），底部会出现一个终端。它自带 `gcloud`，已经登录你的账号。确认项目：

```bash
gcloud config set project 你的项目ID
```

### 2. 取代码

```bash
git clone https://github.com/7CCCCC21X/Donglan.git
cd Donglan
git checkout claude/serene-shannon-ri6od9   # 合并到主分支之后可以省掉这一行
```

仓库是私有的：用户名填 GitHub 用户名，密码处粘贴一个 GitHub **Personal access token**（GitHub → Settings → Developer settings → Fine-grained tokens，给这个仓库 Contents 只读权限）。

### 3. 填设置

```bash
cp ruletaker.env.example ruletaker.env
nano ruletaker.env
```

至少填：`RULE_API_KEY`（或 `PREDICT_API_KEY`）、`PREDICT_PRIVATE_KEY`、`PREDICT_ACCOUNT`（用普通钱包就留空）。**先保持 `RULE_TAKER=paper`**。规则价格、份数可以照 Railway 上的值抄。`Ctrl+O` 回车保存，`Ctrl+X` 退出。

### 4. 一条命令部署

```bash
bash deploy/gcp/rt.sh deploy
```

它会：在 **asia-northeast1-b（东京）** 建一台虚拟机 `ruletaker`（`e2-standard-2`，Ubuntu 24.04，20 GB 盘）→ 上传代码 → 装 Ubuntu 官方源的 Rust 工具链、编译 → 跑全部离线测试（有一个失败就不启动）→ 把设置文件放到虚拟机的 `/etc/ruletaker/ruletaker.env`（权限 640）→ 用 systemd 启动并设成开机自启、崩溃自动重启。第一次大约 8～10 分钟（多数时间在编译）。第一次运行 `gcloud compute ssh` 会问是否生成 SSH 密钥，直接回车。

想换机器或区域：`MACHINE=n2-standard-2 ZONE=asia-northeast1-a bash deploy/gcp/rt.sh deploy`。

### 5. 自检

```bash
bash deploy/gcp/rt.sh check     # 登录、账户、余额、授权、这个区域能不能下单（只读）
bash deploy/gcp/rt.sh latency   # 到 Predict 的延迟（只读）
bash deploy/gcp/rt.sh status    # 状态，就是 huice 的 /rules
```

（设了 Telegram 命令机器人的话，也可以直接给它发 `/check`；`bash deploy/gcp/rt.sh test` 或 `/test` 再做一次真实的挂单测试。）

`check` 会向下单接口发一个**空订单**（内容为空，Predict 只会拒为“无效”，不可能成交）来看这个区域能不能交易：返回 400 是正常的，返回 403 说明这个区域不能下单，要换区域。**请自己确认 Predict.fun 的服务条款允许你在这个地区使用。**

### 6. 切到真实吃单

1. `paper` 跑一两天，`rt.sh status` 看“抢单记录”和每场的结果。
2. Railway 上 `RULE_TAKER=off`，重新部署。
3. 这边：`nano ruletaker.env` 把 `RULE_TAKER=on`，然后

```bash
bash deploy/gcp/rt.sh env
```

4. 部署完把 Cloud Shell 里的 `ruletaker.env` 删掉（里面有私钥，脚本也会提醒）：`shred -u ruletaker.env`。以后改设置用 `bash deploy/gcp/rt.sh ssh` 登录虚拟机，`sudo nano /etc/ruletaker/ruletaker.env`，再 `sudo systemctl restart ruletaker`；规则价格、份数这些直接在网页控制台里改更方便。
5. 运行一次安全加固：`bash deploy/gcp/rt.sh lockdown`（见“[安全](#安全)”）。

### 费用

`e2-standard-2` 东京按需价格大约每月 60 多美元（以控制台显示为准），加上 20 GB 磁盘一两美元。想省钱可以 `MACHINE=e2-medium`（共享核心，约一半价格，偶尔会有几毫秒的 CPU 抖动）。不用了就 `bash deploy/gcp/rt.sh delete`。

---

## 日常操作

都在 Cloud Shell 的仓库目录下运行：

| 命令 | 作用 |
|---|---|
| `bash deploy/gcp/rt.sh lockdown` | **安全加固**（部署后运行一次）：封掉这台机器所有入站连接，SSH 只走 Google 的 IAP 通道；`unlock` 解除。见“[安全](#安全)” |
| `bash deploy/gcp/rt.sh console` | 打开网页控制台和复盘页（见下一节） |
| `bash deploy/gcp/rt.sh status` | 状态（文字版）：模式、推送、账户余额、每秒检查时段、下一个抢单时刻、每条规则的下一场、抢单记录、今天的成交和盈亏 |
| `bash deploy/gcp/rt.sh check` | **自检**：登录、账户、余额、授权、下单接口在这个区域能不能用、读一次多快、推送、记录、场次，最后一句结论（和 Telegram `/check`、控制台“自检”按钮一样） |
| `bash deploy/gcp/rt.sh test` | **挂单测试**：挂一张约 $1、远低于市价的买单，查到后马上撤掉，每一步写 Predict 的回答和往返用时（和 Telegram `/test` 一样） |
| `bash deploy/gcp/rt.sh logs` | 实时日志（`Ctrl+C` 退出） |
| `bash deploy/gcp/rt.sh pause` / `resume` | 暂停（不开新单，挂着的单当场撤）/ 恢复；暂停在重启后仍然有效。今日亏损停机后 `resume` 会从现在的盈亏起再给一个 `RULE_MAX_DAILY_LOSS_USD` |
| `bash deploy/gcp/rt.sh mode paper` | 运行中切换模式（off / paper / on），**不会高于**设置文件里的 `RULE_TAKER` |
| `bash deploy/gcp/rt.sh update` | 代码更新后：`git pull` 再运行它，重新上传、跑测试、重启（设置不动） |
| `bash deploy/gcp/rt.sh env` | 上传改好的 `ruletaker.env` 并重启 |
| `bash deploy/gcp/rt.sh restart` / `stop` / `start` | 重启 / 停止 / 启动服务 |
| `bash deploy/gcp/rt.sh ssh` | 登录虚拟机 |
| `bash deploy/gcp/rt.sh probe asia-east2-a` | 在别的区域临时开一台小机器量延迟，量完自动删除 |

停止或重启时，服务会先等在途订单的应答（最多 25 秒）、撤掉还挂着的单，再退出。

### Telegram：怎么绑定（通知、命令、自检）

**最简单的绑法：给这个服务单独建一个机器人，通知和命令都走它。**

1. 在 Telegram 里找 **@BotFather**，发 `/newbot`，起个名字（例如“规则吃单东京”）和用户名（必须以 `bot` 结尾），它会回一串 Token（`123456:ABC…` 这样的）。
2. 点开新机器人的对话，按 **Start**（机器人不能主动给没和它说过话的人发消息）。
3. 设置文件里填三行：

   ```
   TELEGRAM_COMMAND_TOKEN=新机器人的 Token
   TELEGRAM_COMMANDS=on
   ADMIN_USER_ID=你的数字用户 ID
   ```

   用户 ID 就是 Railway 变量里那个 `ADMIN_USER_ID`，照抄；不记得就给 Railway 的机器人发 `/id`。
4. `bash deploy/gcp/rt.sh env` 上传并重启。几秒后新机器人会发来“🚀 规则吃单已启动”，输入框旁边的“菜单”里有全部命令。发 `/check` 试一下。

这样启动、成交、结算、告警都发到你和新机器人的私聊里，和 Railway 机器人的消息分开。

**想让通知继续发到原来的群话题**（和 huice 的通知放一起）：再加两行——`TELEGRAM_BOT_TOKEN=Railway 那个机器人的 Token`（只用来发消息，和 Railway 不冲突）和 `TELEGRAM_CHAT_ID=群ID:话题ID`。群和话题的 ID 这样查：把新机器人拉进群，在那个话题里发 `/id@新机器人的用户名`，它回“这个聊天：-100…:话题ID”，照抄。命令仍然发给新机器人。

**不要这样填**：把 Railway 机器人的 Token 填进 `TELEGRAM_COMMAND_TOKEN`，或者在 `TELEGRAM_COMMANDS=on` 时只填 Railway 的 `TELEGRAM_BOT_TOKEN`——同一个 Token 只能有一个程序收消息，两边会互相抢，命令都收不全。

| 命令 | 作用 |
|---|---|
| `/check` | **自检**（huice `/live check` 的对应物，内容更多）：模式和停机线、Predict 登录、Predict 账户的控制钥匙（链上核对）、USDT 和 BNB 余额（够不够买满一场）、交易所授权、**下单接口在这个区域能不能用**（发一个空订单，Predict 拒为无效 = 能用，403 = 这个区域不能交易，空订单不可能成交）、读一次 Predict 的往返、推送连接和推送延迟、币安指数（盘中吃单开着时）、数据库和订单日志、接下来一天的场次和市场找到没有、最近问题，最后一句结论 |
| `/test` | **挂单测试**（huice `/live test` 的对应物）：在一个撮合已开启的规则场次（没有的话用 BTC 当前的 15 分钟场次）挂一张买涨的限价单——最高 2¢、低于最高买价 1¢、**绝不高于最低卖价**（不会成交），约 $1，10 分钟后自动过期；按哈希查到它、撤掉、再查最终状态，每一步写 Predict 的回答和往返用时。签名、授权、这个区域能不能下单一次验证完。它不算规则吃单的单，不进记录 |
| `/status`（或 `/rules`） | 状态，和 huice 的 `/rules` 同一个版式：模式、每秒检查时段、下单速度、网络、每条规则的下一场和近 30 天的盘口观察、抢单记录（每场卖单几点几秒出现、多久被吃光、我们的单每一段用了多久）、今天的成交和盈亏。比 huice 多一行账户余额；没有“优先”和“借 Key”那两段（这里只有规则吃单）。一般一条消息发完，超过 Telegram 的字数上限时按行分成两条 |
| `/pause`、`/resume` | 暂停（挂着的单当场撤）/ 恢复 |
| `/mode off\|paper\|on` | 切换模式，不高于设置文件的 `RULE_TAKER` |
| `/id` | 查你的用户 ID 和这个聊天的 ID（任何人都能用） |

只认 `ADMIN_USER_ID` 发来的命令（`/id` 除外），别人发了不理。自检、挂单测试要几秒，机器人先回“自检中…”，结果随后发来。

---

## 控制台和复盘页

一个网页，两个标签：**控制台**和**复盘**。网页在虚拟机上只对本机开放（`127.0.0.1:8080`），不开任何公网端口；从浏览器看要走一条 SSH 通道：

1. 在 Cloud Shell 里运行 `bash deploy/gcp/rt.sh console`（窗口保持开着）。
2. 点 Cloud Shell 工具栏右上角的 **「网页预览」**（方框里一个眼睛的图标）→ **「在端口 8080 上预览」**。浏览器新开一页就是控制台。这个预览地址只有登录了你的 Google 账号才能打开。
3. 看完回到 Cloud Shell 按 `Ctrl+C` 关掉通道。

用自己电脑上的 gcloud 也可以：`gcloud compute ssh ruletaker --zone asia-northeast1-b -- -N -L 8080:127.0.0.1:8080`，然后打开 <http://localhost:8080>。设了 `STATUS_KEY` 时网页会先要口令（存在这个浏览器里）。

**控制台**（每 3 秒刷新）：

- 现在：模式、现在下不下单和原因（暂停、未就绪、日亏损停机、限流、余额……）、账户 USDT（可用余额扣掉了在途订单）、今天已花和已结算盈亏、推送是否连着和推送延迟、每秒检查是否在进行。
- 操作：模式 关 / 只记录 / 真实吃单（**不能高于设置文件的 `RULE_TAKER`**，切到真实吃单要确认）、暂停 / 恢复（暂停时挂着的单当场撤掉；亏损停机后点恢复会从当时的盈亏起再给一个额度）、撤掉全部挂单。
- 每条规则的下一场：价格、下一场的北京时间、对手挂单时刻、它最近一次看盘说了什么（还没发布 / 盘口没有 ≤58¢ 的涨 / 等撮合开启 / 已下单 …）。
- 速度与网络：服务所在区域、主循环延迟、到 Predict 新建连接和读一次的用时、近 30 天真实订单“盘口变化→推送到达→发出→应答”各段中位和最快、预先签好的单用上了几张。
- **设置**：和 huice 控制台“规则吃单”一组一样——黄金 / 原油的开关、每场份数、每条规则的盘前价和盘中价、每天上限、今日亏损停机线、每秒检查时段、对手提前多少分钟。点预设值**立即生效**，也可以填自定义值保存；保存时和启动时一样整体校验，填错了什么都不改并说明原因。网页保存的值重启后仍然有效，标着“网页保存”，点“恢复部署设置”回到设置文件里的值；设置文件后来改了、和网页保存的值冲突时，启动时只删掉冲突的那一项并在 Telegram 告诉你。私钥、Key、链、模式上限这些不能在网页上改。
- 最近问题：最近 12 条读取失败、下单被拒等。

**复盘**（时间范围：近 7 天 / 30 天 / 90 天 / 全部，按场次开场时间算）：

- 真实吃单、只记录各一张总账：已结算盈亏、回报率、赢 / 输场数、买到的份数和成本、待结算。
- 累计盈亏曲线（实线真实、虚线只记录）。
- 按规则：每条规则真实 / 只记录分开的场次、份数、每份成本、盈亏、赢 / 输；其中盘中买的单列一张。
- 盘口观察：每条规则看了几场、几场有规则价以内的卖单、平均可买份数、最低卖价中位、撮合开启和卖单出现在开场前多久、没买到就没了的场次（卖单存活多久）。
- 抢单记录：每一场卖单几点几秒出现（毫秒）、多久开始被吃、多久被吃光、我们买到多少、每张单出现后几毫秒发出、推送延迟、自己用了多久、是不是预先签好的、往返多久。
- 按日期：每天的场次、赢 / 输、成本、当日盈亏、累计。
- 最近的场次：每场买了多少、均价、结果、盈亏。
- **导出 CSV**：所有订单一行一条（真实和只记录都有），Excel 直接打开。

复盘数据算一次只要几毫秒，留 30 秒；抢单时刻前后那几秒不算（给的是上一次的），不会挡住下单。

---

## 提速：具体改了什么

规则吃单拼的是“对手在整刻钟后几秒挂出一排卖单，谁先吃到算谁的”。huice 11-18 的记录：卖单出现后 0.35–0.42 秒被别人吃光；我们的单在出现后 289 ms 发出（推送 178 ms + 机器人自己 111 ms），再加半个往返约 150 ms 才到 Predict。这里每一段都缩短了：

| 环节 | huice（Railway 阿姆斯特丹，共用一个进程） | 本服务（东京，独立进程） |
|---|---|---|
| 盘口变化 → 推送到达 | 中位 178–246 ms | 东京就近接收；部署后 `status` 的“推送延迟中位”就是它 |
| 推送到达 → 请求发出 | 最快 1 ms，但常被主循环占住（111 ms、2098 ms、2540 ms） | Rust 版本机测 0.07 ms（预先签好）/ 0.12 ms（当场签名）；没有别的任务和它抢 |
| 请求 → Predict（半个往返） | 约 150–180 ms（读一次 233–311 ms） | `rt.sh latency` 量；`status` 的“网络”一行持续显示 |

具体做法：

1. **只做规则吃单的独立进程**：没有原策略的整步、预挂的几万条记录和账户快照、复盘页重算、σ 校准，主循环延迟只剩零点几毫秒（`status` 的“速度”一行实时显示）。
2. **推送一到，当场看这一场**：每一场有自己的“正在看”标记，读推送的那个任务直接调用这一场的判断、定份数、签名，并当场把下单请求的字节写进到 Predict 的连接，然后才去读下一帧（不经过调度、也不受调度器的轮转预算限制）。另一场在等 REST 读取，不再挡住这一场（huice 同一时刻只能跑一轮）。
3. **预先签好单**：每秒检查时段和对手挂单时刻（`RULE_LEAD_MINUTES`）前后，每场“这一场还能买的全部份数、按规则价”的那张单提前签好。salt 由场次和第几张单决定，所以预先签的和当场签的是**同一张单**，用上时 `status` 的抢单记录写“预先签好”。
4. **不等上一张的应答**：一张单发出后马上可以看下一次推送；盘口上新出现、超出在途订单份数的卖单立刻另下一张（每场最多 3 张同时在途，份数上限照算在途的）。
5. **连接保温**：忙的时段（开场前 15 分钟、对手挂单时刻、每秒检查时段、盘中）保证 2 秒内用过一条到 Predict 的连接——下单请求只复用 3 秒内用过的连接（否则怕连接已被服务器关掉、订单结果不明），这样下单永远不用重新 TCP+TLS 握手。
6. **下单路上不读链**：交易所授权额度是无限额度时不再在下单前读链（后台每小时核对），登录 token 在后台提前续期，余额在后台读（忙的时段每 20 秒、平时每 2 分钟、每次成交后马上读一次）。
7. **订单不排在读取后面**：下单、撤单有自己的并发名额（读取最多占 4 个，至少留 2 个给订单），限流节拍里订单可以预支，读取慢的时候订单不排队。
8. **更快的编码和签名**：Rust 原生 JSON 解析和 secp256k1 签名（带预计算表，启动时就建好）；订单请求体直接按字段拼成字节；EIP-712 哈希按字段直接拼——和 Python 版、和 eth-account 的通用编码器逐字节一致（测试里核对）。
9. **没有垃圾回收停顿**：Rust 没有垃圾回收（Python 版要在抢单那几秒手动关掉它）；连接复用前先确认服务器没关它，下单请求不会落在一条已断的连接上。
10. **部署在东京**，systemd 里给进程 `Nice=-5`。

没有改的：规则本身、价格、份数、每天上限、开场前 15 秒停止下单、签名在开场前 5 秒到期、1.5 秒后撤掉没成交的部分、60 秒确认没有晚到的成交、409 / 应答丢失按哈希核对绝不重发、手续费按份数扣——全部和 huice 一样。

---

## 为什么是东京、怎么确认

huice 11-18 的网络一行：阿姆斯特丹到 Predict 的入口（Cloudflare）新建连接 7–31 ms，读一次盘口 233–311 ms。入口近、请求慢，说明 Predict 的服务器离欧洲很远，这个数和到东亚的往返吻合。

部署前后都可以量：

```bash
bash deploy/gcp/rt.sh latency                 # 量正在跑的这台（东京）
bash deploy/gcp/rt.sh probe asia-east2-a      # 香港，临时开一台小机器量完删掉
bash deploy/gcp/rt.sh probe asia-northeast3-a # 首尔
bash deploy/gcp/rt.sh probe asia-southeast1-a # 新加坡
```

比较输出里“读盘口 … 中位”那个数：最小的区域离 Predict 最近（下单的往返和它差不多）。

**AWS 还是 Google Cloud？** 对速度起作用的是机器在哪个城市，不是哪家云：两家在东京都有机房，请求都是先到 Cloudflare 的东京入口、再到 Predict。估计两家只差一两毫秒（没有实测过），而从阿姆斯特丹搬到东京省的是几百毫秒。这个仓库的部署脚本是给 Google Cloud 写的（Cloud Shell 一条命令部署、用网页预览看控制台，自己电脑上什么都不用装）；代码本身不挑云，`deploy/gcp/vm-setup.sh` 在任何 Ubuntu 24.04 机器上都能装。建议先在 Google Cloud 东京跑起来看实测；想和 AWS 东京（`ap-northeast-1`）比，在那边开一台小机器跑同一个 `ruletaker latency`（或 Python 版的 `python -m ruletaker latency`），比“读盘口”的中位数。如果别的区域明显更小，`ZONE=那个区域 bash deploy/gcp/rt.sh deploy` 换过去（先 `delete` 旧的，或者用 `VM=ruletaker2` 另起一台再删旧的，注意不要两台同时 `on`）。

---

## 规则（和 huice 相同）

黄金、原油 15 分钟涨跌的几个固定时段，结果不是盘口给的五五开：开场前盘口出现规则价格以下的指定方向，就直接吃单。时段按纽约时间和 CME 的日历算，跟着夏令时走。

| 规则（价格变量） | 场次（纽约时间） | 北京时间（冬令时 / 夏令时） | 买 | 默认 |
|---|---|---|---|---|
| 黄金休市 4 档（`RULE_GOLD_SHUT_CENTS`） | 17:00–17:45，周一至周四 | 周二至周五 06:00–06:45 / 05:00–05:45 | 涨 | 58¢ |
| 黄金开盘第一档（`RULE_GOLD_OPEN_CENTS`） | 18:00，周一至周四 | 周二至周五 07:00 / 06:00 | 跌 | 50¢ |
| 黄金纽约 05:15（`RULE_GOLD_0515_CENTS`） | 05:15，周一至周五 | 18:15 / 17:15 | 跌 | 58¢ |
| 原油休市 4 档（`RULE_OIL_SHUT_CENTS`） | 17:00–17:45，周一至周四 | 同黄金 | 跌 | 0 = 不做 |
| 原油开盘第一档（`RULE_OIL_OPEN_CENTS`） | 18:00，周一至周四 | 同黄金 | 跌 | 50¢ |
| 黄金纽约 05:00（`RULE_GOLD_0500_CENTS`） | 05:00，周一至周五 | 18:00 / 17:00 | 涨 | 0 = 不做 |
| 黄金休市前一档（`RULE_GOLD_1645_CENTS`） | 16:45，周一至周五 | 05:45 / 04:45 | 涨 | 0 = 不做 |
| 原油纽约 00:45（`RULE_OIL_0045_CENTS`） | 00:45，周一至周五 | 13:45 / 12:45 | 跌 | 0 = 不做 |

- **怎么吃**：每场在 Predict 发布前后找到市场（发布约在开场前 24 小时），订阅它的盘口推送；盘口上规则方向有价格不高于规则价的卖单，就下一张限价单：价格 = 规则价；卖单 ≥20 份且不少于这一场还能买的 1/4（对手挂单时刻前后只要 ≥20 份）时，份数 = 这一场还能买的全部份数（它吃到的是到达那一刻盘口上的全部），否则按看到的份数；每场最多 `RULE_*_SHARES` 份，每天最多 `RULE_MAX_DAY_USD`，不超过账户 USDT。不是 post-only，1.5 秒后撤掉没成交的部分，60 秒后确认最终成交。开场前 15 秒起不再下单，签名在开场前 5 秒到期。撮合还没开启时（只收 post-only 挂单）每 0.3 秒左右读一次状态，开启那一刻就买。被拒后这一场 30 秒后再试，拒满 5 次不再下。
- **每秒检查时段**（`RULE_FAST_HOURS`，默认北京 01:00–07:00）和**对手挂单时刻**（`RULE_LEAD_MINUTES`，默认开场前 300 分钟）：每秒看一次、每秒最多 3 次 REST 核对推送有没有漏、提前 1 分钟准备好登录和授权、预先签好单。
- **盘中吃单**（`RULE_GOLD_SHUT_LIVE_CENTS`、`RULE_GOLD_OPEN_LIVE_CENTS`，默认 0 = 不做）：黄金休市 4 档和开盘第一档在开场后接着买，条件是币安 XAUUSDT 合约指数已经站在规则方向（休市 4 档比起点高 0.005% 以上、开盘第一档低 0.02% 以上，起点取起点那根 1 分钟指数 K 的收盘和 Predict 页面的起点价里对规则方向更不利的那个）；开场 3 秒后开始、结束前 15 秒停止，第 1 分钟最高按盘前价格，指数一推到就当场看。需要这台机器能连币安合约行情（`fapi.binance.com`、`fstream.binance.com`），连不上时状态页会写，盘中就不买。
- 回测和每条规则的来历见 huice 的 README“规则吃单”一节。

---

## 设置一览

完整的带说明的模板见 [`ruletaker.env.example`](ruletaker.env.example)。规则相关的变量名、默认值、取值范围和 huice 完全一样。

| 变量 | 默认 | 说明 |
|---|---|---|
| `RULE_TAKER` | `off` | off / paper（只记录）/ on（真实吃单） |
| `LIVE_TRADING_ALLOWED` | 不设 = 允许 | 硬开关：只有不设或写 `on` / `1` / `true` / `yes` 才允许真实下单 |
| `RULE_API_KEY` / `PREDICT_API_KEY` | | Predict API Key（专用的那把填 `RULE_API_KEY`） |
| `PREDICT_PRIVATE_KEY` / `PREDICT_ACCOUNT` | | 签名私钥 / 网站账户的充值地址（普通钱包留空） |
| `PREDICT_CHAIN_ID` / `BSC_RPC_URL` | 56 / 公共节点 | 链和节点 |
| `RULE_*_CENTS`、`RULE_*_LIVE_CENTS` | 见上表 | 每条规则的盘前 / 盘中最高价，0 = 不做 |
| `RULE_GOLD` / `RULE_OIL` | on | 两个市场各自的总开关 |
| `RULE_GOLD_SHARES` / `RULE_OIL_SHARES` | 50 | 每场最多买多少份 |
| `RULE_MAX_DAY_USD` | 0 | 每天（北京时间）最多花多少，0 = 不限 |
| `RULE_MAX_DAILY_LOSS_USD` | 0 | 本服务今天已结算亏到这个数就停开新单，0 = 不限 |
| `RULE_FAST_HOURS` | `01:00-07:00` | 每秒检查时段（北京时间），可多段，`off` 关 |
| `RULE_LEAD_MINUTES` | 300 | 对手提前多少分钟挂单，0 = 不按这个时刻 |
| `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` / `ADMIN_USER_ID` | | 通知（话题写 `群ID:话题ID`） |
| `TELEGRAM_COMMANDS` | off | on = 收管理员命令（`/check` 自检、`/test` 挂单测试等） |
| `TELEGRAM_COMMAND_TOKEN` | 空 | 收命令的机器人（本服务专用）；空 = 用 `TELEGRAM_BOT_TOKEN` 收。只填它、不填 `TELEGRAM_BOT_TOKEN` 时通知也走它 |
| `RULE_NOTIFY` | on | 成交、结算通知；off 只发告警 |
| `STATUS_HOST` / `STATUS_PORT` / `STATUS_KEY` | 127.0.0.1 / 8080 / 空 | 状态页；不在本机监听时必须设口令 |
| `RULE_WS_CONNECTIONS` | 1 | 推送连接条数 1～3，谁先送到用谁的 |
| `PREDICT_RATE_PER_MIN` | 220 | 这把 Key 每分钟最多请求数（Predict 上限 240） |
| `CME_EARLY_DAYS` / `CME_CLOSED_DAYS` | 内置到 2027 年底 | CME 节假日（提前收市 / 全天休市），格式 `2026-12-24,2026-12-31` |
| `STATE_DB` | 虚拟机上 `/var/lib/ruletaker/ruletaker.sqlite3` | 记录数据库 |

---

## 安全

**先说实话**：服务要自动下单，就必须用得到私钥，所以私钥一定在那台机器上，“服务器上的私钥绝对安全”做不到。能做的是两件事：让能碰到它的路尽量少；万一被拿走，损失有上限。

### 私钥放在哪、谁能拿到

| 位置 | 保护 |
|---|---|
| Cloud Shell 里的 `ruletaker.env`（填设置时） | Cloud Shell 的个人磁盘，只有你的 Google 账号进得去；部署完删掉（脚本会提醒） |
| 传到虚拟机的路上 | SSH 加密通道 |
| 虚拟机上的 `/etc/ruletaker/ruletaker.env` | root 所有、权限 640，只有 root 和服务自己的用户读得到；磁盘由 Google 默认加密 |
| 服务进程的内存 | 专用的、不能登录的用户运行；不写 core dump |

**不会出现在**：GitHub（`.gitignore` 排除了 `ruletaker.env`）、日志（只写“有没有设置”）、网页控制台（看不到也改不了）、Telegram 消息、虚拟机的元数据、测延迟用的临时机器（`rt.sh probe` 只带 API Key 过去）。

**谁能拿到**：能登录你 Google 账号的人（能 SSH 进机器、能读磁盘），和能往这个 GitHub 仓库推代码的人（`rt.sh update` 会把仓库里的代码装到机器上运行）。真正的锁是这两个账号。

### 脚本已经替你做的

- 机器**不带服务账号**：就算有人进了机器，也拿不到能操作你 Google Cloud 项目的令牌。
- 服务只往外连（Predict、BNB 链节点、币安、Telegram），**不监听任何公网端口**；控制台只在本机 `127.0.0.1`，要看得走 SSH 通道。
- **`bash deploy/gcp/rt.sh lockdown`**（部署后运行一次）：防火墙封掉这台机器的所有入站连接，SSH 只能走 Google 的 IAP 通道——连接先过 Google 登录和项目权限检查，公网上连不到它的 22 端口。脚本先验证通道能用才封；封完验证失败会自动撤回。之后 `rt.sh` 的所有命令自动走通道（慢一两秒）；`rt.sh unlock` 解除。
- **依赖锁定并校验哈希**：Rust 版的每个依赖包按 `Cargo.lock` 锁定版本，cargo 下载时逐个核对 SHA-256（`--locked`：锁文件和代码对不上就拒绝编译）；编译器来自 Ubuntu 官方源（有签名的软件包，不从网上下载安装脚本）。Python 版（备用）的每个包按 `python/requirements.lock` 的 SHA-256 校验，只装现成的 wheel。
- Shielded VM（安全启动、vTPM、完整性监控）；Ubuntu 默认自动装安全更新。
- 网页控制台的所有修改操作要专用请求头（别的网站的页面发不出来）；私钥、Key 不能在网页或 Telegram 里改；Telegram 只认 `ADMIN_USER_ID`，而且改不了价格和份数。

### 需要你自己做的（按重要程度）

1. **Google 账号开两步验证**，最好用通行密钥或安全密钥（<https://myaccount.google.com/security>）。项目的 IAM 里只留你自己；不要创建、下载服务账号密钥。
2. **这个钱包里只放够用的钱。** 私钥被拿走的最大损失就是账户里的余额：按 `RULE_MAX_DAY_USD` 留几天的量，利润定期提走。和 Railway 共用同一个账户时，同一把私钥存在两个平台上，哪边出事都一样。
3. **GitHub 账号开两步验证**；仓库保持私有，不加不认识的协作者。
4. 部署完运行 `bash deploy/gcp/rt.sh lockdown`，并删掉 Cloud Shell 里的 `ruletaker.env`。私钥不要贴到聊天、截图、工单里。
5. **Telegram 账号开两步验证**（设置 → 隐私和安全）：管理员账号能暂停、恢复、切模式。
6. Google Cloud **设预算提醒**（结算 → 预算和提醒）：云账号被盗最常见的后果是被拿去开一堆机器挖矿。
7. **怀疑私钥泄露时**：马上在 Predict 网站把钱提到别的地址 → `bash deploy/gcp/rt.sh stop` → 换一个新账户。导出的私钥没法“改密码”，只能弃用。

没有做的：把私钥放进硬件密钥（KMS / HSM）里签名——每签一张单要多一次网络往返，和抢单的目标冲突；Secret Manager 对一个人的项目和现在的做法保护程度相同（都取决于 Google 账号）。加固脚本（`lockdown`）和上面的流程用假的 gcloud 模拟验证过顺序和回滚，没有在真实的 Google Cloud 项目上跑过。

---

## 记录、恢复与风控

- **记录**：每场每个模式一条（真实 `rule:<场次链接>`，只记录 `rule:paper:<场次链接>`），存在虚拟机的 SQLite（`/var/lib/ruletaker/`）。格式和 huice 一样：订单哈希、成交份数、均价、手续费、结算结果、盈亏，以及每场的“盘口观察”（卖单什么时候出现、什么时候开始被吃、什么时候被吃光，按 Predict 盘口自己的时间戳）。
- **发单前先落盘**：每张真实订单在请求发出前，先把哈希写进数据库旁边的订单日志（`ruletaker.sqlite3.rule-orders`）。进程在发单途中崩溃或重启，启动时按哈希去 Predict 核对，**绝不重发**。应答丢失、超时、裸 409 都按“结果未知”处理，按哈希找回。
- **风控**：每场份数、每天上限、日亏损停机、`LIVE_TRADING_ALLOWED` 硬开关、暂停（会保存，重启后仍然暂停）；运行中切换模式不能高于设置文件。发单前最后一刻（请求真正发出的那一刻）再核一次：设置是不是刚改过、是否暂停、是否就绪、**离开场是否已不足 15 秒**（盘中：离结束）、余额够不够（扣掉同时在途的其他单）、盘中指数是否还在规则方向。
- **余额**：链上读到的 USDT 扣掉读余额前后下的单可能已经花掉的钱（成交上链要几秒），几场同时有卖单时第二张单按剩下的钱下，不会被 Predict 以余额不足拒绝。
- **数据库写不进去时不开新单**（例如磁盘满）：记录写不进去，重启后就找不到这张单，所以宁可不下；写入会一直重试，恢复后自动继续。
- **停止时**：在途订单最多等 25 秒应答，应答一到就撤掉没成交的部分（systemd 给停止留了 75 秒，平时一秒就停）。
- **私钥和账号**：见上一节“[安全](#安全)”。
- **地区和条款**：Predict 不向受限地区开放交易（受限地区读取正常、下单 403）。部署后先跑 `rt.sh check`；是否允许在当地使用请以 Predict.fun 的服务条款为准。

---

## 和 huice 相比没搬过来的

- **自动领取**（链上领取赢的份额）：不在这里做。同一个账户时由 Railway 上的 huice 领；单独账户请到网站上领取。
- **避开预挂自己的挂单**（huice 的 `own_asks`）：预挂不在这个进程里，看不到它的账本。默认开着的几条规则所在的场次，预挂本来就不挂（1.150.1 / 1.151.1 / 1.154.19），所以没有影响；见上面“同一个账户”里关于 05:00、16:45、原油 00:45 的说明。
- **把 Key 借给盘口和预挂**、**让原策略和预挂让路**：这里只有规则吃单，不需要。

---

## 本地运行与测试

Rust 版（需要 Rust 1.85 以上：Ubuntu 24.04 上 `sudo apt install cargo-1.97`，或用 rustup）：

```bash
cargo test                                            # 全部离线测试（不联网、不要 Key）
cargo test --release bench_hot_path -- --ignored --nocapture   # 推送帧 → 下单请求交给网络 的耗时
cargo run --release -- plan                           # 接下来两天每条规则的场次（北京 / 纽约时间）
cargo run --release -- check --env ruletaker.env      # 只读自检
cargo run --release -- run --env ruletaker.env
```

Python 版（备用，在 `python/` 目录里）：

```bash
cd python
python3 -m venv venv && venv/bin/pip install --require-hashes --only-binary :all: -r requirements.lock   # 需要 Python 3.12 以上
venv/bin/python -m unittest discover -s tests
venv/bin/python tests/bench_hotpath.py
```

测试覆盖（Rust 版）：时段和 CME 日历（夏令时、周末、节假日，两年逐场和 Python 版对照）、盘口换算（含 1 − 0.42 这种必须精确的比较）、签名和请求体（和 Python 版逐字节一致、能还原签名人）、一张单从推送到结算的全过程（整场一张单、在途订单旁边再下一张、读成交、撤单、晚到成交、结算盈亏）、每天上限和余额、暂停和亏损停机、被拒、应答丢失按哈希找回、409、发单途中设置变了、发出那一刻再核开场前 15 秒、停止时撤掉在途单、跟单不碰在途单、撮合没开启时等待、预先签名、订单日志崩溃恢复、只记录模式、数据库写不进去时不下单、自检、挂单测试、网页控制台的接口（设置保存 / 校验 / 恢复、模式不高于部署设置、没有 X-RT 头的 POST 一律拒绝、STATUS_KEY）、Telegram 命令和 /id、HTTP 连接复用（服务器悄悄关掉的连接在发单前就被发现、POST 绝不重发、gzip、chunked、重定向）、推送的版本和合并、币安指数、SQLite 读写、复盘统计和 CSV、主循环从启动到停止。

代码结构：

| 文件 | 内容 |
|---|---|
| `src/taker/` | 规则吃单本体：`look.rs`（看盘、定份数、签名、下单）、`orders.rs`（跟单、撤单、结算）、`flow.rs`（主循环、节奏、预热、核对、盘中）、`control.rs`（暂停 / 模式 / 自检 / 挂单测试 / 网页设置）、`text.rs`（状态文字） |
| `src/rules.rs` | 规则和场次、盘口换算、记录的统计、抢单记录 |
| `src/stream.rs` | Predict 盘口推送 |
| `src/predict.rs` | Predict REST 接口、限流节拍、链上读余额和授权、应答解析 |
| `src/wallet.rs` | 订单签名（EIP-712，Predict 账户的 Kernel 包装） |
| `src/index.rs` | 币安合约指数（盘中吃单用） |
| `src/http.rs` | 长连接 HTTP/1.1（TLS、连接池、断连检测、POST 绝不重发） |
| `src/store.rs` | SQLite 记录（和 Python 版同一个表、同一种格式） |
| `src/telegram.rs`、`src/server.rs` | Telegram、状态页和控制台的接口 |
| `src/console.rs`、`src/review.rs`、`web/console.html` | 控制台的设置、复盘的统计和 CSV、网页本身 |
| `src/config.rs`、`src/clock.rs`、`src/cards.rs` | 设置、纽约时间和 CME 日历、15 分钟市场 |
| `src/tests.rs`、`src/parity.rs`、`tests/vectors/` | 全过程测试、和 Python 版逐字节对照的数据 |
| `deploy/gcp/` | Google Cloud 部署脚本（`rt.sh`，含安全加固 `lockdown`）、systemd 服务、虚拟机安装脚本 |
| `Cargo.toml`、`Cargo.lock` | Rust 依赖；全部依赖的版本和哈希锁定 |
| `python/` | Python 版 2.0（备用）：`python/ruletaker/`、测试、`requirements.lock` |

"""python -m ruletaker [run | latency | check | plan] [--env FILE]

  run       the service (the default): the rule taker, its Telegram notifications and its status page
  latency   read-only: how far Predict is from this machine (compare regions before moving; no order is sent)
  check     read-only: the settings, the sign-in, the Predict account, the balance, the allowances, and whether Predict's
            order endpoint answers this region (an empty order body: refused as invalid, never placed)
  plan      no network: the rules' windows of the next two days in Beijing and New York time"""
from __future__ import annotations

import argparse
import asyncio
import contextlib
import logging
import os
import signal
import statistics
import sys
import time
import urllib.parse
from typing import Any

from . import VERSION
from .config import Config, Stored, key_of, load_env_file
from .util import LOG, bj, clean_error


def setup_logging(level: str) -> None:
    logging.basicConfig(level=getattr(logging, level, logging.INFO), stream=sys.stdout,
                        format="%(asctime)s.%(msecs)03d %(levelname)s %(name)s: %(message)s", datefmt="%Y-%m-%d %H:%M:%S")


def gce_zone(timeout: float = 1.0) -> str:
    """The Compute Engine zone this runs in (the metadata server), "" elsewhere."""
    import urllib.request
    try:
        req = urllib.request.Request("http://metadata.google.internal/computeMetadata/v1/instance/zone", headers={"Metadata-Flavor": "Google"})
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        with opener.open(req, timeout=timeout) as answer:
            return answer.read().decode().rsplit("/", 1)[-1]
    except Exception:
        return ""


def load_env(env_file: str | None) -> dict:
    env = dict(os.environ)
    for path in [env_file] if env_file else ["/etc/ruletaker/ruletaker.env", ".env"]:
        if path and os.path.isfile(path):
            env = load_env_file(path, env)
            break
    return env


def load_config(env_file: str | None) -> Config:
    return Config.from_env(load_env(env_file))


def make_telegram(config: Config, handler: Any) -> Any:
    """The notifications through TELEGRAM_BOT_TOKEN's bot — or, with none set, through the command bot (one bot for both);
    the commands through TELEGRAM_COMMAND_TOKEN's bot, else the notifying one's."""
    from .notify import Telegram
    notify_token = config.telegram_token or config.telegram_command_token
    return Telegram(notify_token, config.telegram_chats, config.admin_user_id, config.telegram_commands,
                    handler=handler, enabled=bool(notify_token), command_token=config.telegram_command_token)


async def run_service(config: Config, env: dict | None = None) -> int:
    from .index import IndexFeed
    from .rules import RULES
    from .status import StatusServer
    from .store import Store
    from .taker import RuleTaker

    LOG.info("规则吃单 %s 启动｜%s", VERSION, config.secrets_line())
    store = Store(config.state_db)
    region = config.region or await asyncio.to_thread(gce_zone) or os.environ.get("RAILWAY_REPLICA_REGION", "")
    # the index feed of every rule that can buy in its windows' course (gold): made always — an in-play price may be set on the
    # control page while the service runs — and connected only while one is set (RuleTaker.index_wanted)
    index = IndexFeed(sorted({r.spec.symbol for r in RULES if r.lead > 0}), config.binance_fapi, config.binance_ws)
    taker = RuleTaker(config, store, index=index, base_env={k: v for k, v in (env or {}).items() if k.startswith("RULE_")} if env is not None else None)
    taker.region = region
    telegram = make_telegram(config, taker.command)
    telegram.menu_head = taker.menu_head
    telegram.settings_view = taker.settings_view
    taker.telegram = telegram
    status = StatusServer(taker, config.status_host, config.status_port, config.status_key)
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        with contextlib.suppress(NotImplementedError, RuntimeError):
            loop.add_signal_handler(sig, taker.stop.set)
    with contextlib.suppress(Exception):
        await status.start()
    tg = asyncio.create_task(telegram.run(), name="telegram")
    running = asyncio.create_task(taker.run(), name="taker")

    async def announce() -> None:  # once signed in (real orders) or after 20 s: what the operator sees first is the state it settled in
        for _ in range(40):
            if taker.settings.mode != "on" or taker.ready or running.done():
                break
            await asyncio.sleep(0.5)
        telegram.send(f"🚀 规则吃单 {VERSION} 已启动（{region or '未知区域'}）：{taker.mode_text()}")
    told = asyncio.create_task(announce(), name="announce")
    if taker.settings_dropped:
        telegram.send("⚠️ 控制台保存的这些设置和设置文件冲突，已删掉（改用设置文件的值）：" + "、".join(taker.settings_dropped))
        LOG.warning("控制台保存的设置和设置文件冲突，已删掉：%s", "、".join(taker.settings_dropped))
    try:
        await running
    finally:
        told.cancel()
        telegram.send("🛑 规则吃单服务停止")
        telegram.stop()
        with contextlib.suppress(Exception):
            await asyncio.wait_for(tg, 8)
        await status.stop()
        store.flush(10)
    return 0


PUSH_SECONDS = 45.0  # how long the probe watches the pushed books for their delay


async def index_delays(url: str, seconds: float) -> list[float]:
    """The index the in-play rules buy on (Binance XAUUSDT markPrice, pushed every second) for ``seconds``: each push's delay
    from Binance's own event time to here (an error when no connection is had)."""
    import json
    from .stream import ws_connect
    began, lags = time.perf_counter(), []
    ws = await ws_connect(url, {})
    try:
        while (left := began + seconds - time.perf_counter()) > 0:
            try:
                frame = json.loads(await asyncio.wait_for(ws.recv(), left))
            except asyncio.TimeoutError:
                break
            data = frame.get("data") if isinstance(frame, dict) and isinstance(frame.get("data"), dict) else frame
            stamp = data.get("E") if isinstance(data, dict) else None
            if isinstance(data, dict) and data.get("e") == "markPriceUpdate" and isinstance(stamp, (int, float)) and stamp > 0:
                lag = time.time() * 1000 - stamp
                if -1_000 <= lag < 60_000:
                    lags.append(lag)
    finally:
        await ws.close()
    return lags


def book_sig(data: Any) -> str:
    """A book's top five levels a side, as text: the same book read over REST and pushed compares equal."""
    from .predict import predict_levels
    data = data if isinstance(data, dict) else {}
    side = lambda key, bids: ",".join(f"{p.normalize()}@{q.normalize()}" for p, q in predict_levels(data.get(key), bids)[:5])
    return f"{side('bids', True)}|{side('asks', False)}"


def fastest(xs: list[float]) -> str:
    """The fastest and the 10th percentile of a list of delays: "最快 8 ms、10% 分位 15 ms"."""
    v = sorted(xs)
    return f"最快 {v[0]:.0f} ms、10% 分位 {v[int((len(v) - 1) * 0.1)]:.0f} ms"


def delays(xs: list[float]) -> str:
    """"中位 66 ms（最快 41，九成在 90 以内）": the push delays' median, fastest and 90th percentile."""
    v = sorted(xs)
    return f"中位 {statistics.median(v):.0f} ms（最快 {v[0]:.0f}，九成在 {v[int((len(v) - 1) * 0.9)]:.0f} 以内）"


async def latency(config: Config, rounds: int = 10, push_seconds: float = PUSH_SECONDS, base: str = "", ws_url: str = "") -> dict:
    """Read-only: a first request (a new connection: its handshake), ``rounds`` reads of an order book on that connection, the
    push connection's handshake, its ping there and back, and for ``push_seconds`` the delay of every change Predict pushes of
    the BTC quarter-hour books (its own timestamp to here) — what a race is decided on before an order leaves. Compare the push
    delay between regions: the smaller, the nearer to Predict's books."""
    from . import http
    from .predict import API_BASES, PREDICT_REST, READ_POOL, predict_markets
    from .stream import PREDICT_WS_URL, ws_connect
    base = base or API_BASES.get(config.chain_id, PREDICT_REST)  # (``base`` / ``ws_url``: stand-ins, for the tests)
    host = urllib.parse.urlsplit(base).hostname or ""
    headers = {"x-api-key": config.api_key} if config.api_key else {}
    spread = lambda xs: f"中位 {statistics.median(xs):.1f} ms（最快 {min(xs):.1f}，最慢 {max(xs):.1f}）" if xs else "没量到"
    zone = config.region or await asyncio.to_thread(gce_zone)
    found: dict = {"region": zone, "reads": [], "pings": [], "first": None, "ws_open": None, "ws_book": None, "lags": [], "index": []}
    print(f"Predict 网络延迟探测（只读，不下单）｜区域 {zone or '未知'}｜版本 {VERSION}" + ("" if config.api_key else "｜没有设置 API Key：可能被拒绝"))
    print("（约 1 分钟，其间占这把 Key 每分钟额度的四分之一左右：别在抢单时段跑）")
    loop = asyncio.get_running_loop()
    now = int(time.time() * 1000)
    slug, market_id = f"btc-updown-15m-{(now - now % 900_000) // 1000}", ""
    try:
        began = time.perf_counter()
        answer = await loop.run_in_executor(READ_POOL, http.request_json, f"{base}/categories/{slug}", None, 10, headers)
        found["first"] = (time.perf_counter() - began) * 1000
        markets = predict_markets(answer)
        market_id = markets[0]["id"] if markets else ""
        print(f"第一次请求（含新建连接）{found['first']:.0f} ms｜市场 {slug} → {market_id or '没找到'}")
    except Exception as error:
        print(f"读市场失败：{clean_error(error) or type(error).__name__}")
    watched = [market_id] if market_id else []
    try:  # (the next quarter hour's market too: twice the books to time)
        nxt = f"btc-updown-15m-{(now - now % 900_000) // 1000 + 900}"
        more = predict_markets(await loop.run_in_executor(READ_POOL, http.request_json, f"{base}/categories/{nxt}", None, 10, headers))
        watched += [str(more[0]["id"])] if more and more[0].get("id") else []
    except Exception:
        pass
    path = f"/markets/{market_id}/orderbook" if market_id else f"/categories/{slug}"
    why = ""
    refused: list[float] = []
    for _ in range(rounds):
        began = time.perf_counter()
        try:
            await loop.run_in_executor(READ_POOL, http.request, base + path, None, 10, headers)
            found["reads"].append((time.perf_counter() - began) * 1000)
        except Exception as error:
            why = clean_error(error) or type(error).__name__
            if why.startswith("HTTP "):
                refused.append((time.perf_counter() - began) * 1000)
        await asyncio.sleep(0.4)
    net = http.HTTP_NET.get(host) or {}
    connect = list(net.get("connect") or ())
    print((f"读盘口 {len(found['reads'])} 次（同一条连接）：{spread(found['reads'])}" if found["reads"] or not refused else
           f"读盘口被拒绝（{why}），{len(refused)} 次被拒的应答来回 {spread(refused)}——也是一次往返，可以拿来比")
          + (f"｜新建连接（TCP+TLS）{spread(connect)}" if connect else "") + (f"｜入口 {net['edge']}" if net.get("edge") else ""))
    # Predict's push connection, and at the same time the Binance index the in-play rules buy on, and the first watched book
    # read over REST (a quarter of the key's budget) over the very seconds the pushes are watched: which shows a new book first
    index = asyncio.ensure_future(index_delays(f"{config.binance_ws}/stream?streams=xauusdt@markPrice@1s", push_seconds))
    polled = watched[0] if watched else ""
    ws_first: dict = {}
    rest_first: dict = {}
    poll_s = max(0.1, 60 / (0.25 * max(config.rate_per_min, 1.0)))
    watching, watched_out = asyncio.Event(), asyncio.Event()

    async def rest_reads() -> tuple[int, int, list[float]]:
        reads = stamped = 0
        ages: list[float] = []
        await asyncio.wait([asyncio.ensure_future(watching.wait()), asyncio.ensure_future(watched_out.wait())],
                           return_when=asyncio.FIRST_COMPLETED)
        last = ""
        while polled and not watched_out.is_set():
            asked = time.perf_counter()
            try:
                answer = await loop.run_in_executor(READ_POOL, http.request_json, f"{base}/markets/{polled}/orderbook", None, 10, headers)
            except Exception:
                answer = None
            if isinstance(answer, dict):
                got = time.perf_counter()
                reads += 1
                data = answer.get("data") if isinstance(answer.get("data"), dict) else answer
                stamp = data.get("updateTimestampMs")
                stamp = stamp if isinstance(stamp, (int, float)) and stamp > 0 else 0
                stamped += stamp > 0
                sig = book_sig(data)
                if sig != last:  # (a book new since the last read: when it first showed here, how old Predict says it was)
                    rest_first.setdefault(sig, (got, not last))
                    age = time.time() * 1000 - stamp
                    if stamp and last and -1_000 <= age < 60_000:
                        ages.append(age)
                    last = sig
            await asyncio.sleep(max(0.0, poll_s - (time.perf_counter() - asked)))
        return reads, stamped, ages
    rest = asyncio.ensure_future(rest_reads())
    try:
        began = time.perf_counter()
        ws = await ws_connect(ws_url or PREDICT_WS_URL, headers)
        found["ws_open"] = (time.perf_counter() - began) * 1000
        try:
            for _ in range(5):
                began = time.perf_counter()
                await asyncio.wait_for(await ws.ping(), 5)
                found["pings"].append((time.perf_counter() - began) * 1000)
                await asyncio.sleep(0.2)
            if watched:
                # every change of the watched books for push_seconds: its delay from Predict's own timestamp to here (the first
                # book of a market is the book as it stands, not a change: not timed — as the service times its pushes)
                import json
                began = time.perf_counter()
                for i, mid in enumerate(watched, 1):
                    await ws.send(json.dumps({"method": "subscribe", "requestId": i, "params": [f"predictOrderbook/{mid}"]}))
                print(f"看 {len(watched)} 个 BTC 15 分钟盘口 {push_seconds:.0f} 秒，量推送延迟…")
                watching.set()
                seen: set = set()
                while (left := began + push_seconds - time.perf_counter()) > 0:
                    try:
                        frame = json.loads(await asyncio.wait_for(ws.recv(), left))
                    except asyncio.TimeoutError:
                        break
                    if not isinstance(frame, dict) or frame.get("type") != "M":
                        continue
                    topic = str(frame.get("topic") or "")
                    if topic == "heartbeat":
                        await ws.send(json.dumps({"method": "heartbeat", "data": frame.get("data")}))
                        continue
                    if not topic.startswith("predictOrderbook/"):
                        continue
                    if found["ws_book"] is None:
                        found["ws_book"] = (time.perf_counter() - began) * 1000
                    stamp = (frame.get("data") or {}).get("updateTimestampMs")
                    mid = topic.split("/", 1)[1]
                    if mid == polled:
                        ws_first.setdefault(book_sig(frame.get("data")), (time.perf_counter(), not ws_first))
                    if mid in seen and isinstance(stamp, (int, float)) and stamp > 0:
                        lag = time.time() * 1000 - stamp
                        if -1_000 <= lag < 60_000:
                            found["lags"].append(lag)
                    seen.add(mid)
        finally:
            watched_out.set()
            await ws.close()
        print(f"推送连接：握手 {found['ws_open']:.0f} ms｜ping 来回 {spread(found['pings'])}"
              + (f"｜订阅到第一份盘口 {found['ws_book']:.0f} ms" if found["ws_book"] is not None else ""))
        lags = found["lags"]
        print("推送延迟：没量到（盘口这段时间没变，或推送没连上）：换个时段再量" if not lags else
              f"推送延迟：盘口只变了 {len(lags)} 次，量不准（{delays(lags)}）：换个时段再量" if len(lags) < 5 else
              f"推送延迟（Predict 盘口时间戳 → 这里收到，{len(lags)} 次盘口变化）：{delays(lags)}")
    except Exception as error:
        print(f"推送连接失败：{clean_error(error) or type(error).__name__}")
    watched_out.set()
    try:
        found["index"] = xs = await index
        print(f"币安指数：{push_seconds:.0f} 秒只收到 {len(xs)} 次，量不准：换个时段再量" if len(xs) < 5 else
              f"币安指数推送延迟（XAUUSDT 每秒一次，币安时间戳 → 这里收到，{len(xs)} 次）：{delays(xs)}")
    except Exception as error:
        print(f"币安指数：连不上（{clean_error(error) or type(error).__name__}）——这个区域做不了盘中规则（币安合约不对部分地区开放）")
    rest_count, stamped, rest_ages = await rest
    # the same book seen both ways (neither side's first book: that is the book as it stood, not a change)
    rest_ahead, push_ahead = [], []
    for sig, (at_rest, first_rest) in rest_first.items():
        if sig in ws_first and not first_rest and not ws_first[sig][1]:
            lead = (ws_first[sig][0] - at_rest) * 1000
            (rest_ahead if lead >= 0 else push_ahead).append(abs(lead))
    found.update(rest_ages=rest_ages, rest_ahead=rest_ahead, push_ahead=push_ahead)
    lags = found["lags"]
    if rest_count:
        print(f"REST 读盘口（每 {poll_s:.2f} 秒一次，{rest_count} 次）：" + (
            "答复里没有盘口时间戳" if not stamped else "盘口没变过" if not rest_ages else
            f"新盘口从 Predict 时间戳到这里 {fastest(rest_ages)}（含读的间隔，看最快的）；推送 {fastest(lags) if lags else '没量到'}"))
        lead = lambda xs: f"{len(xs)} 次" + (f"（中位早 {statistics.median(xs):.0f} ms）" if xs else "")
        if rest_ahead or push_ahead:
            print(f"同一份盘口两边都看到 {len(rest_ahead) + len(push_ahead)} 次：REST 先到 {lead(rest_ahead)}，推送先到 {lead(push_ahead)}")
        fresher = len(rest_ages) >= 5 and lags and min(rest_ages) + 15 < min(lags)
        print("→ REST 的盘口比推送新：开场前那几秒加快 REST 读取有用（设置 RULE_VERIFY_MS，见 README）"
              if fresher or (len(rest_ahead) >= 5 and len(rest_ahead) > 2 * len(push_ahead)) else
              "→ REST 不比推送新：加快 REST 读取没用，RULE_VERIFY_MS 不用改" if len(rest_ages) >= 5 or len(rest_ahead) + len(push_ahead) >= 5 else
              "→ 样本太少，判断不了：换个盘口活跃的时段再量")
    limits = (http.HTTP_NET.get(host) or {}).get("limits", "")
    print(f"接口限额（Predict 答复里写的）：{limits}——按它设 PREDICT_RATE_PER_MIN（留一成余量）" if limits else
          "接口限额：Predict 的答复里没有写，按 Predict 给这把 Key 的额度设 PREDICT_RATE_PER_MIN（留一成余量）")
    print("怎么看：抢单时卖单从出现到我们看到，就是“推送延迟”这一段（东京实测中位 66～85 ms）——比区域主要看它，越小越近。"
          "盘中规则另看“币安指数推送延迟”：换区域时它也会变，两项一起比。"
          "“读盘口”代表不了下单：东京读一次约 12 ms，下单往返约 49 ms（下单在 Predict 那边走得更远）。下单往返只能用私钥量："
          "在候选区域临时部署一台只记录模式的机器跑 rt.sh test（README“为什么是东京、怎么确认”）。各区域请在同一时段量。")
    return found


async def check(config: Config, probe_order: bool = True) -> int:
    from .predict import API_BASES, PREDICT_REST, RPC_URLS, Chain, PredictApi, http_status
    from .wallet import Wallet, exchange_key, from_wei, native_signing
    print(f"规则吃单 {VERSION} 自检（只读）")
    print(config.secrets_line())
    s = config.rules
    print(f"模式 RULE_TAKER={s.mode}｜黄金 {'开' if s.on('gold') else '关'} 每场 {s.shares_of('gold')} 份｜原油 {'开' if s.on('oil') else '关'} 每场 {s.shares_of('oil')} 份"
          f"｜每天上限 ${s.max_day_usd:g}｜日亏损停机 ${config.max_daily_loss_usd:g}｜签名 {'coincurve 原生' if native_signing() else '纯 Python'}")
    if not config.private_key:
        print("没有 PREDICT_PRIVATE_KEY：只能 off / paper。")
        return 0
    wallet = Wallet(config.private_key, config.chain_id, config.account)
    print(f"钱包：{wallet.kind}｜下单账户 {wallet.maker}" + (f"｜签名钱包 {wallet.signer}" if wallet.predict_account else ""))
    api = PredictApi(API_BASES.get(config.chain_id, PREDICT_REST), config.api_key, wallet, config.rate_per_min)
    chain = Chain(config.rpc_url or RPC_URLS.get(config.chain_id, RPC_URLS[56]), wallet)
    ok = True
    try:
        await api.ensure_auth()
        print("✅ Predict 登录成功")
    except Exception as error:
        ok = False
        print(f"❌ Predict 登录失败：{clean_error(error)}")
    if wallet.predict_account:
        try:
            owner = await chain.account_owner(wallet.predict_account)
            same = owner.lower() == wallet.signer.lower()
            ok &= same
            print("✅ 私钥是这个 Predict 账户的控制钥匙" if same else f"❌ 私钥不是这个 Predict 账户的控制钥匙（链上记录 {owner}）")
        except Exception as error:
            ok = False
            print(f"❌ 链上核对 Predict 账户失败：{clean_error(error)}")
    try:
        usdt, bnb = await chain.usdt_balance(), await chain.bnb_balance()
        print(f"余额：USDT {from_wei(usdt):,.2f}｜签名钱包 BNB {from_wei(bnb):.4f}")
    except Exception as error:
        print(f"⚠️ 余额没读到：{clean_error(error)}")
    # (all four: which one a market settles on is its isNegRisk × isYieldBearing; the service's /check names the ones in use)
    for key in (exchange_key(n, y) for y in (False, True) for n in (False, True)):
        try:
            allowance = await chain.allowance(key)
            print(f"授权 {key}：{'✅ 已授权' if allowance >= 10 ** 20 else '❌ 没有授权（在 Predict 网站用这个账户交易一次）'}")
        except Exception as error:
            print(f"⚠️ 授权 {key} 没读到：{clean_error(error)}")
    if probe_order and api.fresh():
        # an empty order: Predict refuses it as invalid (400) — unless this region may not trade at all (403). Never placed
        try:
            await api.authed("POST", "/orders", {"data": {}}, priority=True)
            print("⚠️ 下单接口对空订单没有报错（意外），请人工核对")
        except Exception as error:
            status = http_status(error)
            if status == 403:
                ok = False
                print(f"❌ 下单接口 403：这个区域可能不允许交易（{clean_error(error)}）")
            else:
                print(f"✅ 下单接口在这个区域可用（空订单被拒为无效：HTTP {status or '?'}，这是预期的）")
    print("结论：" + ("可以开 RULE_TAKER=on" if ok else "先把上面的 ❌ 解决"))
    return 0 if ok else 1


def plan(config: Config, days: int = 2) -> int:
    from .clock import et_clock
    from .rules import RULES, live_price_of, live_rule, windows
    now = int(time.time() * 1000)
    s = config.rules
    print(f"接下来 {days} 天的规则场次（北京时间｜纽约时间）；RULE_TAKER={s.mode}，RULE_LEAD_MINUTES={s.lead_minutes}")
    for rule in RULES:
        price, live = s.price(rule.key), live_price_of(s, rule)
        state = (f"≤{price}¢" if live_rule(s, rule) else "盘前不买") + (f"，盘中 ≤{live}¢" if live else "")
        if not live_rule(s, rule) and not live:
            state = "不做"
        starts = windows(rule, now, now + days * 86_400_000)
        print(f"· {rule.name}（买{rule.word}，{state}）：" + ("、".join(f"{bj(t)}｜{et_clock(t)}" for t in starts) or "没有"))
    return 0


async def key_command(env: dict, from_stdin: bool) -> int:
    """``key``: a private key's signing address, never the key itself — the one on standard input (--stdin), else the one systemd
    hands the service (kept encrypted on this machine: deploy/gcp/vm-key.sh). With a settings file naming a PREDICT_ACCOUNT,
    whether the key controls that account, on chain. The address alone on the first line (a script reads it); exit 0 (it does,
    or no account to check), 1 (it does not), 2 (no key, or not a key), 3 (the chain could not be read)."""
    from .predict import RPC_URLS, Chain
    from .wallet import Wallet
    if from_stdin:
        raw = sys.stdin.readline()
    else:
        try:
            raw = Stored.from_systemd().key
        except ValueError as error:
            print(f"❌ {error}", file=sys.stderr)
            return 2
    key = key_of(raw)
    if not key:
        print("❌ 这不是私钥：应为 64 位十六进制（0x 可省）" if key is None else
              "❌ 没有私钥" + ("（标准输入是空的）" if from_stdin else "（这台机器上没有加密保存的私钥）"), file=sys.stderr)
        return 2

    def setting(name: str) -> str:
        return str(env.get(name, "")).strip()
    try:
        chain_id = int(setting("PREDICT_CHAIN_ID") or 56)
    except ValueError:
        chain_id = 56
    try:
        wallet = Wallet(key, chain_id, setting("PREDICT_ACCOUNT"))
    except Exception as error:
        print(f"❌ {clean_error(error) or error}", file=sys.stderr)
        return 2
    print(wallet.signer, flush=True)
    if not wallet.predict_account:
        return 0
    chain = Chain(setting("BSC_RPC_URL").rstrip("/") or RPC_URLS.get(chain_id, RPC_URLS[56]), wallet)
    try:
        owner = await chain.account_owner(wallet.predict_account)
    except Exception as error:
        print(f"⚠️ 链上核对没做成：{clean_error(error)}")
        return 3
    if owner.lower() == wallet.signer.lower():
        print(f"✅ 是 Predict 账户 {wallet.predict_account} 的控制钥匙（链上核对）")
        return 0
    print(f"❌ 不是 Predict 账户 {wallet.predict_account} 的控制钥匙（链上记录的是 {owner}）")
    return 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="python -m ruletaker", description="规则吃单 · 独立服务")
    parser.add_argument("command", nargs="?", default="run", choices=["run", "latency", "check", "plan", "key"],
                        help="key：只读，私钥的签名地址（从不显示私钥本身）——--stdin 从标准输入读一把，否则用这台机器加密保存的那把；"
                             "给了 --env 且设了 PREDICT_ACCOUNT，再在链上核对它是不是这个 Predict 账户的控制钥匙")
    parser.add_argument("--env", default=None, help="环境变量文件（默认 /etc/ruletaker/ruletaker.env 或 ./.env；key 只在给了时才读）")
    parser.add_argument("--no-order-probe", action="store_true", help="check 时不向下单接口发空订单")
    parser.add_argument("--stdin", action="store_true", help="key 时从标准输入读私钥")
    args = parser.parse_args(argv)
    if args.command == "key":
        # (the settings only when named: the key alone needs none)
        return asyncio.run(key_command(load_env(args.env) if args.env else {}, args.stdin))
    try:
        env = load_env(args.env)
        # the private key kept encrypted on this machine, as systemd hands it to the service (none outside systemd)
        config = Config.load(env, Stored.from_systemd())
    except ValueError as error:
        print(f"设置有误：{error}", file=sys.stderr)
        return 2
    setup_logging(config.log_level)
    if args.command == "plan":
        return plan(config)
    if args.command == "latency":
        asyncio.run(latency(config))
        return 0
    if args.command == "check":
        return asyncio.run(check(config, not args.no_order_probe))
    return asyncio.run(run_service(config, env))


if __name__ == "__main__":
    sys.exit(main())

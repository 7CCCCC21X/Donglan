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
from .config import Config, load_env_file
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


async def latency(config: Config, rounds: int = 10) -> dict:
    """Read-only: a first request (a new connection: its handshake), ``rounds`` reads of an order book on that connection, the
    push connection's handshake, its ping there and back and a subscription's first book. Compare the "读盘口" median between
    regions: the smaller, the nearer to Predict."""
    from . import http
    from .predict import API_BASES, PREDICT_REST, READ_POOL, predict_markets
    from .stream import PREDICT_WS_URL, ws_connect
    base = API_BASES.get(config.chain_id, PREDICT_REST)
    host = urllib.parse.urlsplit(base).hostname or ""
    headers = {"x-api-key": config.api_key} if config.api_key else {}
    spread = lambda xs: f"中位 {statistics.median(xs):.1f} ms（最快 {min(xs):.1f}，最慢 {max(xs):.1f}）" if xs else "没量到"
    zone = config.region or await asyncio.to_thread(gce_zone)
    found: dict = {"region": zone, "reads": [], "pings": [], "first": None, "ws_open": None, "ws_book": None}
    print(f"Predict 网络延迟探测（只读，不下单）｜区域 {zone or '未知'}｜版本 {VERSION}" + ("" if config.api_key else "｜没有设置 API Key：可能被拒绝"))
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
    try:
        began = time.perf_counter()
        ws = await ws_connect(PREDICT_WS_URL, headers)
        found["ws_open"] = (time.perf_counter() - began) * 1000
        try:
            for _ in range(5):
                began = time.perf_counter()
                await asyncio.wait_for(await ws.ping(), 5)
                found["pings"].append((time.perf_counter() - began) * 1000)
                await asyncio.sleep(0.2)
            if market_id:
                import json
                began = time.perf_counter()
                await ws.send(json.dumps({"method": "subscribe", "requestId": 1, "params": [f"predictOrderbook/{market_id}"]}))
                while time.perf_counter() - began < 5:
                    frame = json.loads(await asyncio.wait_for(ws.recv(), 5))
                    if isinstance(frame, dict) and frame.get("type") == "M" and str(frame.get("topic") or "").startswith("predictOrderbook/"):
                        found["ws_book"] = (time.perf_counter() - began) * 1000
                        stamp = (frame.get("data") or {}).get("updateTimestampMs")
                        if isinstance(stamp, (int, float)) and stamp > 0:
                            found["ws_age"] = time.time() * 1000 - stamp
                        break
        finally:
            await ws.close()
        print(f"推送连接：握手 {found['ws_open']:.0f} ms｜ping 来回 {spread(found['pings'])}"
              + (f"｜订阅到第一份盘口 {found['ws_book']:.0f} ms" if found["ws_book"] is not None else ""))
    except Exception as error:
        print(f"推送连接失败：{clean_error(error) or type(error).__name__}")
    print("怎么看：“读盘口”的中位数就是这里到 Predict 的一次往返（下单也差不多）。在东京（asia-northeast1）量一次，"
          "和原来 Railway 上 /rules 网络一行的“读一次”比——小得多就说明搬对了。")
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
    for neg in (False, True):
        key = exchange_key(neg, False)
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


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="python -m ruletaker", description="规则吃单 · 独立服务")
    parser.add_argument("command", nargs="?", default="run", choices=["run", "latency", "check", "plan"])
    parser.add_argument("--env", default=None, help="环境变量文件（默认 /etc/ruletaker/ruletaker.env 或 ./.env）")
    parser.add_argument("--no-order-probe", action="store_true", help="check 时不向下单接口发空订单")
    args = parser.parse_args(argv)
    try:
        env = load_env(args.env)
        config = Config.from_env(env)
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

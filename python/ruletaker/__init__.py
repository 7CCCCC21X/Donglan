"""规则吃单 · 独立服务（Predict.fun 黄金 / 原油 15 分钟涨跌的固定时段规则吃单）。

从 huice（TG 合约昨收偏离提醒机器人）1.154.27 的 rules.py 拆出来的独立进程：只做规则吃单，不和原策略、预挂、
复盘页共用一个事件循环，部署在离 Predict 近的地方（Google Cloud 东京 asia-northeast1），盘口一推到就在同一回合里
判断、签单、发单。逻辑、记录格式和保护措施沿用 huice 的规则吃单；改动和提速见 README.md。
"""
VERSION = "2.0.0"
SOURCE = "huice 1.154.27 rules.py"

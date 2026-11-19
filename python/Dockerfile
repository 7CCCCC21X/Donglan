# Python 版（备用）的容器：docker build -t ruletaker-py python/（在仓库根目录运行，构建目录是 python/）。
# Google Cloud 上推荐 deploy/gcp/rt.sh 直接装在虚拟机上（systemd）。
FROM python:3.12-slim
ENV PYTHONUNBUFFERED=1 PYTHONDONTWRITEBYTECODE=1 STATE_DB=/data/ruletaker.sqlite3
WORKDIR /app
COPY requirements.lock ./
# 每个包按 requirements.lock 里的哈希校验，只装现成的 wheel
RUN pip install --no-cache-dir --require-hashes --only-binary :all: -r requirements.lock
COPY ruletaker ./ruletaker
COPY tests ./tests
# 离线测试（不联网、不要 Key）：有一个失败就构建失败
RUN python -m unittest discover -s tests -q && rm -rf tests
VOLUME ["/data"]
CMD ["python", "-u", "-m", "ruletaker", "run"]

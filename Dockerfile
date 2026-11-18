# 容器方式（可选）：Google Cloud 上推荐 deploy/gcp/rt.sh 直接装在虚拟机上（systemd），这里给本地测试或别的平台用。
FROM python:3.12-slim
ENV PYTHONUNBUFFERED=1 PYTHONDONTWRITEBYTECODE=1 STATE_DB=/data/ruletaker.sqlite3
WORKDIR /app
COPY requirements.txt ./
RUN pip install --no-cache-dir -r requirements.txt
COPY ruletaker ./ruletaker
COPY tests ./tests
# 离线测试（不联网、不要 Key）：有一个失败就构建失败
RUN python -m unittest discover -s tests -q && rm -rf tests
VOLUME ["/data"]
CMD ["python", "-u", "-m", "ruletaker", "run"]

# Python Kafka client for the drivers; driver and control client come from files:.
FROM docker.io/library/python:3.13-alpine@sha256:2d9aefe2fef018a7eb2c13064c89c71929800fd2e5dccdbf52ea5da5bb8d929a
RUN printf 'kafka-python==2.2.20 --hash=sha256:d24fd670a18130c5062b363486d4d97eba79e1f7c436f35ef6bc25330786ef64\n' > /tmp/requirements.txt \
 && pip install --no-cache-dir --only-binary=:all: --require-hashes -r /tmp/requirements.txt \
 && rm /tmp/requirements.txt
ENV PYTHONUNBUFFERED=1

FROM docker.io/library/python:3.13-alpine
WORKDIR /workload
COPY . .
ENV PYTHONUNBUFFERED=1
ENTRYPOINT ["python3", "/workload/main.py"]

FROM docker.io/library/python:3.13-alpine
WORKDIR /workload
COPY target.py oracle.py ./
ENTRYPOINT ["python3", "/workload/target.py"]

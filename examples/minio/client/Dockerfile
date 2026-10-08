FROM docker.io/library/python@sha256:2986c55feb36e6cae00fa1fefb454283e4b33f35e75ff8bdd123b134130be301
ENV PYTHONHASHSEED=0 PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1
COPY wheels/ /wheels/
COPY requirements.lock /requirements.lock
RUN python -m pip install --no-index --find-links=/wheels --require-hashes --no-cache-dir -r /requirements.lock
COPY cas.py /app/cas.py
ENTRYPOINT ["python", "/app/cas.py"]

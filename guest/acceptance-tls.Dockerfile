FROM docker.io/library/alpine:3.22
RUN apk add --no-cache python3 openssl
# Mint during image preparation, with today's notBefore and a one-day lifetime.
RUN openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=localhost \
    -addext subjectAltName=DNS:localhost -keyout /key.pem -out /cert.pem \
    && chmod 0644 /key.pem
COPY tls-probe.py /tls-probe.py
USER 123:456
ENTRYPOINT ["python3", "/tls-probe.py"]

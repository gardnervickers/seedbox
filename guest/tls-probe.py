"""Validate a certificate minted during preparation through a real TLS handshake."""
import socket
import ssl
import threading
import time

cert = ssl._ssl._test_decode_cert('/cert.pem')
issued = ssl.cert_time_to_seconds(cert['notBefore'])
assert 0 <= time.time() - issued < 86400, (time.time(), cert)
server = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
server.load_cert_chain('/cert.pem', '/key.pem')
client = ssl.create_default_context(cafile='/cert.pem')
listener = socket.socket()
listener.bind(('127.0.0.1', 0))
listener.listen(1)
errors = []


def serve():
    try:
        connection, _ = listener.accept()
        with server.wrap_socket(connection, server_side=True) as connection:
            assert connection.recv(4) == b'ping'
            connection.sendall(b'pong')
    except BaseException as error:
        errors.append(error)


worker = threading.Thread(target=serve)
worker.start()
with socket.create_connection(listener.getsockname()) as connection:
    with client.wrap_socket(connection, server_hostname='localhost') as connection:
        connection.sendall(b'ping')
        assert connection.recv(4) == b'pong'
worker.join()
listener.close()
assert not errors, errors
print('same-day TLS certificate validated')

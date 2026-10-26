"""Reply with this machine's hostname and role."""
import os
import socket

with socket.socket() as listener:
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("0.0.0.0", 7000))
    listener.listen()
    while True:
        connection, _ = listener.accept()
        with connection:
            reply = f"{socket.gethostname()} {os.environ.get('ROLE', 'primary')}\n"
            connection.sendall(reply.encode())

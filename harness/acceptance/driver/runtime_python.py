from concurrent.futures import ProcessPoolExecutor
import multiprocessing
import resource
import socket


def square(value):
    return value * value


if __name__ == "__main__":
    assert socket.gethostname() == "python"
    assert resource.getrlimit(resource.RLIMIT_NOFILE) == (1048576, 1048576)
    with ProcessPoolExecutor(max_workers=2, mp_context=multiprocessing.get_context("spawn")) as pool:
        assert list(pool.map(square, range(8))) == [value * value for value in range(8)]
    print("Python process pool passed", flush=True)

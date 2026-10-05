"""Linux x86-64 ABI error numbers, independent of the simulator host OS.

Reference: Linux include/uapi/asm-generic/errno{,-base}.h.
Never import host errno in a guest syscall model.
"""

ENOENT = 2
EIO = 5
EBADF = 9
EAGAIN = 11
EACCES = 13
EFAULT = 14
EEXIST = 17
EINVAL = 22
EFBIG = 27
ESPIPE = 29
ENOTSOCK = 88
EDESTADDRREQ = 89
EMSGSIZE = 90
EADDRINUSE = 98
EADDRNOTAVAIL = 99
ETIMEDOUT = 110

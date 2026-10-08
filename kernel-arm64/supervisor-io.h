/* SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note */
#ifndef SEEDBOX_SUPERVISOR_IO_H
#define SEEDBOX_SUPERVISOR_IO_H
#include <linux/ioctl.h>
#include <linux/types.h>
struct seedbox_io {
    __u16 port;
    __u8 width;
    __u8 write;
    __u32 value;
};
#define SEEDBOX_IO_TRANSFER _IOWR('S', 0xf4, struct seedbox_io)
#endif

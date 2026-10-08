// SPDX-License-Identifier: GPL-2.0
/* Dedicated deterministic supervisor registers, never raw guest memory. */
#include <linux/fs.h>
#include <linux/io.h>
#include <linux/miscdevice.h>
#include <linux/module.h>
#include <linux/of.h>
#include <linux/platform_device.h>
#include <linux/sched.h>
#include <linux/sizes.h>
#include <linux/uaccess.h>
#include "supervisor-io.h"

struct seedbox_io_device {
	struct miscdevice misc;
	void __iomem *registers;
};
static int seedbox_io_open(struct inode *inode, struct file *file)
{
	/* The supervisor is PID 1. An inherited descriptor also cannot be used
	 * by a workload: every transfer repeats this check. */
	return task_pid_nr(current) == 1 ? 0 : -EPERM;
}
static long seedbox_io_ioctl(struct file *file, unsigned int command, unsigned long argument)
{
	struct miscdevice *misc = file->private_data;
	struct seedbox_io_device *device = container_of(misc, struct seedbox_io_device, misc);
	struct seedbox_io request;
	void __iomem *address;
	if (task_pid_nr(current) != 1)
		return -EPERM;
	if (command != SEEDBOX_IO_TRANSFER)
		return -ENOTTY;
	if (copy_from_user(&request, (void __user *)argument, sizeof(request)))
		return -EFAULT;
	if (request.write > 1)
		return -EINVAL;
	if (request.port == 0xf4 || request.port == 0xf5) {
		if (!request.write || request.width != 1)
			return -EINVAL;
	} else if (request.port == 0xf6 || request.port == 0xf7) {
		if (!request.write || request.width != 4)
			return -EINVAL;
	} else if (request.port == 0xf8) {
		if (request.width != 4)
			return -EINVAL;
	} else {
		return -EINVAL;
	}
	address = device->registers + request.port * 8;
	if (request.write) {
		if (request.width == 1)
			writeb(request.value, address);
		else
			writel(request.value, address);
	} else {
		request.value = readl(address);
		if (copy_to_user((void __user *)argument, &request, sizeof(request)))
			return -EFAULT;
	}
	return 0;
}
static const struct file_operations seedbox_io_operations = {
	.owner = THIS_MODULE,
	.open = seedbox_io_open,
	.unlocked_ioctl = seedbox_io_ioctl,
};
static int seedbox_io_probe(struct platform_device *platform)
{
	struct seedbox_io_device *device;
	struct resource *resource = platform_get_resource(platform, IORESOURCE_MEM, 0);
	if (!resource || resource_size(resource) != SZ_16K)
		return -EINVAL;
	device = devm_kzalloc(&platform->dev, sizeof(*device), GFP_KERNEL);
	if (!device)
		return -ENOMEM;
	device->registers = devm_ioremap_resource(&platform->dev, resource);
	if (IS_ERR(device->registers))
		return PTR_ERR(device->registers);
	device->misc = (struct miscdevice) {
		.minor = MISC_DYNAMIC_MINOR,
		.name = "seedbox-io",
		.fops = &seedbox_io_operations,
		.mode = 0600,
	};
	return misc_register(&device->misc);
}
static const struct of_device_id seedbox_io_match[] = {
	{ .compatible = "seedbox,supervisor-io-v1" }, {}
};
static struct platform_driver seedbox_io_driver = {
	.probe = seedbox_io_probe,
	.driver = { .name = "seedbox-io", .of_match_table = seedbox_io_match },
};
builtin_platform_driver(seedbox_io_driver);

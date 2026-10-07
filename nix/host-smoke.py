#!/usr/bin/env python3
"""Installed smoke adapter: active-pair guard and job-local builder storage."""
import fcntl
import json
import os
from pathlib import Path
import stat
import sys


def builder_environment(root, runtime):
    """OCI builders inherit this from the CLI; no host-global Podman storage."""
    config = root / 'storage.conf'
    config.write_text('[storage]\ndriver = "vfs"\n' +
        'graphroot = ' + json.dumps(str(root / 'containers')) + '\n' +
        'runroot = ' + json.dumps(str(runtime)) + '\n')
    engine = root / 'containers.conf'
    engine.write_text('[engine]\ncgroup_manager = "cgroupfs"\nevents_logger = "file"\n' +
        'static_dir = ' + json.dumps(str(root / 'containers/libpod')) + '\n' +
        'tmp_dir = ' + json.dumps(str(runtime / 'libpod')) + '\n' +
        'volume_path = ' + json.dumps(str(root / 'containers/volumes')) + '\n')
    temporary = root / 'tmp'
    temporary.mkdir(mode=0o700)
    return {'CONTAINERS_STORAGE_CONF': str(config), 'CONTAINERS_CONF': str(engine),
            'TMPDIR': str(temporary), 'BUILDAH_ISOLATION': 'chroot'}


def main():
    # This script lives beside the installed amd.py/owned.py, not the PR copy.
    directory = Path(__file__).resolve().parent
    sys.path.insert(0, str(directory))
    import amd
    class RunnerCampaign(amd.Campaign):
        def __init__(self, args, root, deadline):
            super().__init__(args, root, deadline)
            self.env.update(builder_environment(root, Path(os.environ['RUNTIME_DIRECTORY'])))
    amd.Campaign = RunnerCampaign
    cpu = sys.argv[sys.argv.index('--cpu') + 1]
    guard = Path(os.environ['SEEDBOX_HOST_PAIR_GUARDS']) / ('active-' + cpu)
    fd = os.open(guard, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            raise RuntimeError('nonregular active pair guard')
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return amd.main()
    finally:
        os.close(fd)


if __name__ == '__main__':
    raise SystemExit(main())

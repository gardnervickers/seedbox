#!/usr/bin/env python3
"""Fetch only named primary-source files for W4's shared-lock JDK/Jedis pins."""
import hashlib
import json
import pathlib
import sys
import urllib.error
import urllib.parse
import urllib.request

JDK_TAG = "jdk-21.0.12.1+1"
JDK_FILES = [
    "src/hotspot/os/linux/os_linux.cpp",
    "src/hotspot/os/posix/os_posix.cpp",
    "src/hotspot/cpu/x86/rdtsc_x86.cpp",
    "src/hotspot/cpu/x86/rdtsc_x86.hpp",
    "src/hotspot/cpu/x86/vm_version_x86.cpp",
    "src/hotspot/cpu/x86/vm_version_x86.hpp",
    "src/hotspot/os_cpu/linux_x86/vm_version_linux_x86.cpp",
    "src/hotspot/os_cpu/linux_x86/os_linux_x86.inline.hpp",
    "src/hotspot/share/utilities/ticks.cpp",
    "src/hotspot/share/runtime/globals.hpp",
    "src/hotspot/share/runtime/os.cpp",
    "src/hotspot/share/runtime/synchronizer.cpp",
    "src/hotspot/share/runtime/safepointMechanism.inline.hpp",
    "src/hotspot/share/compiler/compilationPolicy.cpp",
    "src/hotspot/share/compiler/compileBroker.cpp",
    "src/hotspot/share/opto/library_call.cpp",
    "src/java.base/unix/classes/sun/security/provider/NativePRNG.java",
    "src/java.base/share/classes/sun/security/provider/SeedGenerator.java",
    "src/java.base/share/classes/java/security/SecureRandom.java",
    "src/java.base/share/classes/java/util/Random.java",
    "src/java.base/share/classes/java/util/Collections.java",
    "src/java.base/share/classes/sun/security/provider/SunEntries.java",
    "src/java.base/share/classes/jdk/internal/util/random/RandomSupport.java",
    "src/java.base/share/classes/java/util/concurrent/ThreadLocalRandom.java",
    "src/java.base/share/conf/security/java.security",
]
JEDIS_FILES = [
    "pom.xml",
    "src/main/java/redis/clients/jedis/Connection.java",
    "src/main/java/redis/clients/jedis/Jedis.java",
    "src/main/java/redis/clients/jedis/DefaultJedisClientConfig.java",
    "src/main/java/redis/clients/jedis/DefaultJedisSocketFactory.java",
]

def main():
    out = pathlib.Path(sys.argv[1])
    out.mkdir(parents=True, exist_ok=True)
    manifest = {
        "scope": "source_audit_not_Linux_runtime_admission",
        "jdk_linux_tag": JDK_TAG,
        "jdk_linux_version": "21.0.12.1+1",
        "jdk_mac_preparation_version": "Zulu21.0.11",
        "qualification": "W4 proposed jdk21/Jedis5.2.0; exact per-platform JDK derives from research/env lock; mac runtime differs from Linux guest",
        "jedis_tag": "v5.2.0",
        "files": [],
    }
    for project, tag, paths in [
        ("openjdk/jdk21u", JDK_TAG, JDK_FILES),
        ("redis/jedis", "v5.2.0", JEDIS_FILES),
    ]:
        for path in paths:
            url = "https://raw.githubusercontent.com/" + project + "/" + urllib.parse.quote(tag, safe="") + "/" + path
            record = {"project": project, "path": path, "url": url}
            try:
                with urllib.request.urlopen(url, timeout=30) as response:
                    content = response.read()
                target = out / project.split("/")[-1] / path
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(content)
                record.update(status="downloaded", bytes=len(content), sha256=hashlib.sha256(content).hexdigest())
            except urllib.error.HTTPError as error:
                record.update(status="HTTP_error", code=error.code)
            manifest["files"].append(record)
    (out / "source-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    failed = [item for item in manifest["files"] if item["status"] != "downloaded"]
    print(json.dumps({"downloaded": len(manifest["files"]) - len(failed), "failed": failed}))
    if failed:
        raise SystemExit(1)

if __name__ == "__main__":
    main()

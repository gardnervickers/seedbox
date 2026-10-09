#!/usr/bin/env python3
"""Validate server-side storage cleanup before uploading; clean canceled uploads."""
import argparse
import json
import os
import subprocess

PREFIX = "pv-images/"


def validate(lifecycle, versioning):
    if versioning.get("Status") in ("Enabled", "Suspended"):
        raise RuntimeError("staging bucket must never have versioning enabled")
    expiration = abort = False
    for rule in lifecycle.get("Rules", []):
        if rule.get("Status") != "Enabled":
            continue
        match = rule.get("Filter", {"Prefix": rule.get("Prefix", "")})
        # Rules conditional on object tags do not cover every staged object.
        if match not in ({}, {"Prefix": ""}, {"Prefix": PREFIX}):
            continue
        expiration |= rule.get("Expiration", {}).get("Days") == 1
        abort |= rule.get("AbortIncompleteMultipartUpload", {}).get("DaysAfterInitiation") == 1
    if not expiration or not abort:
        raise RuntimeError("staging needs verified one-day expiration AND one-day incomplete-multipart cleanup")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=["check", "cleanup"])
    parser.add_argument("--object")
    args = parser.parse_args()
    bucket = os.environ["SPACES_BUCKET"]
    if not bucket:
        raise RuntimeError("SPACES_BUCKET is required")
    command = ["aws", "--endpoint-url", f"https://{os.environ['SPACES_REGION']}.digitaloceanspaces.com",
               "s3api"]

    def aws(operation, *arguments):
        raw = subprocess.check_output([*command, operation, "--bucket", bucket, *arguments,
                                       "--output", "json"], timeout=60)
        return json.loads(raw) if raw.strip() else {}

    if args.command == "check":
        validate(aws("get-bucket-lifecycle-configuration"), aws("get-bucket-versioning"))
        return
    if not args.object or not args.object.startswith(PREFIX):
        parser.error("cleanup requires an object within pv-images/")
    aws("delete-object", "--key", args.object)
    # Ordinary DeleteObject does not delete unfinished multipart uploads.
    for upload in aws("list-multipart-uploads", "--prefix", args.object).get("Uploads", []):
        if upload["Key"] == args.object:
            aws("abort-multipart-upload", "--key", args.object, "--upload-id", upload["UploadId"])


if __name__ == "__main__":
    main()

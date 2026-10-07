"""Application-owned MinIO qualification and deliberate 412 handling bug."""
import argparse
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from threading import Barrier, BrokenBarrierError, Lock
import json
import sys
import time
import boto3
from botocore.config import Config
from botocore.exceptions import ClientError, EndpointConnectionError, ReadTimeoutError

BUCKET = "seedbox-cas"
A = b"acknowledged-value-A"
B = b"conflicting-value-B"
OUTPUT_LOCK = Lock()

def emit(kind, **receipt):
    # These are this application's diagnostics, opaque to Seedbox.
    with OUTPUT_LOCK:
        print(kind + " " + json.dumps(receipt, sort_keys=True), flush=True)

class QualificationError(Exception):
    pass

def client(endpoint, access_key, secret_key):
    return boto3.session.Session().client(
        "s3", endpoint_url=endpoint, region_name="us-east-1",
        aws_access_key_id=access_key, aws_secret_access_key=secret_key,
        config=Config(
            signature_version="s3v4", connect_timeout=1, read_timeout=2,
            retries={"mode": "standard", "total_max_attempts": 1},
            s3={"addressing_style": "path"},
            request_checksum_calculation="when_required",
            response_checksum_validation="when_required"))

def observed(call, **arguments):
    try:
        response = call(**arguments)
    except ClientError as error:
        response = error.response
    status = response["ResponseMetadata"]["HTTPStatusCode"]
    emit("MINIO_HTTP", operation=call.__name__, key=arguments.get("Key"),
         status=status, condition={key: arguments[key] for key in ("IfMatch", "IfNoneMatch")
                                  if key in arguments}, etag=response.get("ETag"),
         value_hex=arguments["Body"].hex() if "Body" in arguments else None)
    return status, response

def require_status(actual, expected):
    if actual != expected:
        raise QualificationError(f"expected HTTP {expected}, received {actual}")

def ready(s3):
    for attempt in range(8):
        try:
            status, _ = observed(s3.list_buckets)
            if status == 200:
                return
            if status not in (500, 503):
                raise QualificationError(f"readiness returned HTTP {status}")
        except (EndpointConnectionError, ReadTimeoutError):
            if attempt == 7:
                raise
        if attempt != 7:
            time.sleep(0.25)
    raise QualificationError("finite readiness budget exhausted")

def create_bucket(s3):
    status, _ = observed(s3.create_bucket, Bucket=BUCKET)
    require_status(status, 200)

def put(s3, key, value, **condition):
    return observed(s3.put_object, Bucket=BUCKET, Key=key, Body=value, **condition)

def get(s3, key):
    status, response = observed(s3.get_object, Bucket=BUCKET, Key=key)
    require_status(status, 200)
    try:
        value, etag = response["Body"].read(), response["ETag"]
        emit("MINIO_GET", key=key, value_hex=value.hex(), etag=etag)
        return value, etag
    finally:
        response["Body"].close()

def require_value(s3, key, expected):
    actual, etag = get(s3, key)
    if actual != expected:
        raise QualificationError(f"GET {key}: unexpected stored bytes")
    return etag

def concurrent_creates(make_client):
    start = Barrier(2, timeout=2)
    def contender(value):
        try:
            s3 = make_client()
        except Exception as error:
            start.abort()
            raise QualificationError("conditional-create client setup failed") from error
        try:
            start.wait()
        except BrokenBarrierError as error:
            raise QualificationError("conditional-create start rendezvous failed") from error
        status, _ = put(s3, "concurrent", value, IfNoneMatch="*")
        return status, value
    with ThreadPoolExecutor(max_workers=2) as workers:
        first = workers.submit(contender, A)
        second = workers.submit(contender, B)
        return [first.result(), second.result()]

def qualify(s3, make_client):
    status, response = put(s3, "matrix", A, IfNoneMatch="*")
    require_status(status, 200)
    original_etag = response["ETag"]
    if require_value(make_client(), "matrix", A) != original_etag:
        raise QualificationError("initial returned/stored ETags differ")
    status, _ = put(s3, "matrix", B, IfNoneMatch="*")
    require_status(status, 412)
    require_value(make_client(), "matrix", A)
    status, response = put(s3, "matrix", B, IfMatch=original_etag)
    require_status(status, 200)
    updated_etag = response["ETag"]
    stored_etag = require_value(make_client(), "matrix", B)
    if updated_etag != stored_etag or updated_etag == original_etag:
        raise QualificationError("returned/stored ETags do not reflect changed tiny body")
    status, _ = put(s3, "matrix", A, IfMatch=original_etag)
    require_status(status, 412)
    require_value(make_client(), "matrix", B)
    status, _ = put(s3, "missing", A, IfMatch=original_etag)
    require_status(status, 404)
    status, _ = observed(make_client().get_object, Bucket=BUCKET, Key="missing")
    require_status(status, 404)
    outcomes = concurrent_creates(make_client)
    if sorted(status for status, _ in outcomes) != [200, 412]:
        raise QualificationError(f"concurrent create outcomes: {outcomes!r}")
    winner = next(value for status, value in outcomes if status == 200)
    require_value(make_client(), "concurrent", winner)
    emit("MINIO_RESULT", mode="qualification", passed=True)
    return 0

def planted(s3, make_client, mode, ledger):
    status, _ = put(s3, "slot", A, IfNoneMatch="*")
    require_status(status, 200)
    ledger.write_bytes(A)
    status, _ = put(s3, "slot", B, IfNoneMatch="*")
    require_status(status, 412)
    if mode == "broken":
        # Planted client bug: drop the condition after an explicit conflict.
        status, _ = put(s3, "slot", B)
        require_status(status, 200)
    actual, _ = get(make_client(), "slot")
    passed = actual == ledger.read_bytes()
    emit("MINIO_RESULT", mode=mode, passed=passed,
         acknowledged_hex=ledger.read_bytes().hex(), stored_hex=actual.hex())
    return 0 if passed else 1

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("qualification", "broken", "fixed"))
    parser.add_argument("--endpoint", required=True)
    parser.add_argument("--access-key", required=True)
    parser.add_argument("--secret-key", required=True)
    parser.add_argument("--ledger", type=Path, default=Path("/tmp/acknowledged-value"))
    arguments = parser.parse_args()
    make_client = lambda: client(arguments.endpoint, arguments.access_key, arguments.secret_key)
    try:
        s3 = make_client()
        ready(s3)
        create_bucket(s3)
        if arguments.mode == "qualification":
            return qualify(s3, make_client)
        return planted(s3, make_client, arguments.mode, arguments.ledger)
    except Exception as error:
        print(f"qualification/setup error: {type(error).__name__}: {error}", file=sys.stderr)
        return 2

if __name__ == "__main__":
    sys.exit(main())

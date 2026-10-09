"""Conservative launch reservations, serialized by the cloud workflow group.

These are admission limits, not a provider-enforced billing cap. Reservations
are persisted before a paid request and never refunded after failure/cancellation.
"""
import datetime
from decimal import Decimal, ROUND_CEILING
import re

MONTHLY_COMPUTE_CENTS = 2000
MAX_HOURLY_USD = Decimal("0.10")
MAX_MONTHLY_IMAGE_BUILDS = 8
MAX_DAILY_IMAGE_BUILDS = 2
RESERVE = "seedbox-ci-reserve-"
BUILD = "seedbox-ci-build-"


def date(now):
    return datetime.datetime.fromtimestamp(now, datetime.timezone.utc).strftime("%Y%m%d")


def reserve(api, attempt, now, *, image=False, seconds=7500):
    if not re.fullmatch(r"[0-9]+-[0-9]+", attempt):
        raise ValueError("reservation needs a numeric GitHub run ID and attempt")
    day = date(now)
    tags = [t["name"] for t in api.list("/tags", "tags")]
    if image:
        prefix = BUILD
        pattern = re.compile(r"seedbox-ci-build-(\d{8})-\d+-\d+$")
        entries = []
        for tag in tags:
            if tag.startswith(prefix):
                match = pattern.fullmatch(tag)
                if not match:
                    raise RuntimeError("malformed image budget reservation; inspect ledger")
                entries.append(match[1])
        if entries.count(day) >= MAX_DAILY_IMAGE_BUILDS:
            raise RuntimeError("daily image-build budget exhausted (2 attempts)")
        if sum(value[:6] == day[:6] for value in entries) >= MAX_MONTHLY_IMAGE_BUILDS:
            raise RuntimeError("monthly image-build budget exhausted (8 attempts)")
        name = f"{prefix}{day}-{attempt}"
    else:
        prefix = RESERVE
        cents = int((MAX_HOURLY_USD * Decimal(seconds) / Decimal(36)).to_integral_value(
            rounding=ROUND_CEILING))
        pattern = re.compile(r"seedbox-ci-reserve-(\d{8})-(\d+)-\d+-\d+$")
        # Charge both months if the bounded lifetime crosses a billing boundary.
        days = sorted({day, date(now + seconds)})
        days = list({value[:6]: value for value in days}.values())
        booked = {value[:6]: 0 for value in days}
        for tag in tags:
            if tag.startswith(prefix):
                match = pattern.fullmatch(tag)
                if not match or int(match[2]) <= 0:
                    raise RuntimeError("malformed compute budget reservation; inspect ledger")
                if match[1][:6] in booked:
                    booked[match[1][:6]] += int(match[2])
        if any(value + cents > MONTHLY_COMPUTE_CENTS for value in booked.values()):
            raise RuntimeError("monthly $20 Droplet-compute reservation budget exhausted")
        name = f"{prefix}{day}-{cents}-{attempt}"
    if any(tag.startswith(prefix) and tag.endswith("-" + attempt) for tag in tags):
        raise RuntimeError("this workflow attempt already reserved a paid operation; rerun explicitly")
    # A failed/lost response stops execution; do not create the paid resource.
    if image:
        api.request("POST", "/tags", {"name": name})
    else:
        for value in days:
            api.request("POST", "/tags", {"name": f"{prefix}{value}-{cents}-{attempt}"})
    return name


def check_size(api, slug, region):
    size = next((s for s in api.list("/sizes", "sizes") if s["slug"] == slug), None)
    if size is None or not size.get("available") or region not in size.get("regions", []):
        raise RuntimeError("requested Droplet size is unavailable in this region")
    rate = Decimal(str(size["price_hourly"]))
    if not rate.is_finite() or not 0 < rate <= MAX_HOURLY_USD:
        raise RuntimeError("requested Droplet exceeds the $0.10/hour price ceiling")
    return rate

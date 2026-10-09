# Ephemeral GitHub Actions PV runners

`PV runner image` builds a complete NixOS DigitalOcean image on a GitHub-hosted
Linux runner. `PV droplet tests` boots one image as a single-job Actions runner,
runs portable prerequisites and the existing `linux-apps` suite in PV mode,
uploads bounded diagnostics, and destroys the droplet. Nothing has been deployed
by adding these files; normal workflows require `PV_CI_ENABLED=true`.

## Startup

The disk contains the development shell closure, Rust toolchain, vendored Cargo
dependencies for all three workspaces, GitHub runner, Podman, and prebuilt guest
kernel/helpers. NixOS roots these in the system closure. Boot does not install
packages, update channels, or rebuild the system. Tests use the resident nixpkgs
source with `nix develop --offline`; Cargo uses the local vendor directory offline.
Source compilation still occurs per revision. Application container images and
build layers can still be pulled/built during acceptance tests.

Dependency changes on main trigger an image build. An existing matching image
skips the expensive build and upload. The dependency key is only a CI cache key;
it never enters witnesses, run identities, or replay admission. There is no
weekly rebuild of unchanged dependencies. Update `flake.lock` to update the Nix
GitHub runner package; rebuilding the same lock does not upgrade it.

The workflow always runs cloud control from main. `test_ref` selects the commit
executed on the disposable worker. Its dependency key must have a prepared image;
merge dependency changes and build that image before testing dependent revisions.
Creation-to-registration time is reported in the job summary. Boot speed and
nested KVM support require live validation, not just Nix evaluation.

## Billing controls

These controls govern this CI's requests; they are not a DigitalOcean account
spending cap. Do not use the CI credentials for other provisioning code.

| Resource | Control |
| --- | --- |
| Droplet compute | Current API price must be at most $0.10/hour. Each launch reserves 21 cents before creation, covering two hours plus five minutes at that ceiling. |
| Monthly compute | $20 of reservations per UTC calendar month: at most 95 launches, including canaries and failed/canceled attempts. No refunds. A launch crossing a month boundary reserves in both months. |
| Concurrency | Image, canary and test workflows share `seedbox-pv-cloud`; the controller also refuses when a managed droplet exists. This relies on using these serialized workflows, not an atomic cloud-wide lock. |
| Droplet lifetime | Two-hour expiration attached atomically at creation; independent cleanup every minute. Local runner limit 110 minutes; test job timeout 100 minutes. |
| Image builds | At most two attempts/day and eight/month; each build job has a 120-minute timeout. Reuse matching images. |
| Image storage | Maximum two managed images, including pending imports. Before importing a replacement, delete the oldest unused completed image and confirm its absence. Stop if both slots are protected; never delete an in-use image. Failed/pending imports expire after two hours, even if GitHub was canceled. |
| Upload staging | Maximum 8 GiB compressed image. Require an unversioned bucket with verified one-day object expiration and incomplete multipart abort rules before building/uploading. |
| Diagnostics | At most 64 MiB/job, 8 MiB/file, 128 files, retained seven days. Full VM disks are omitted; export them separately when needed. |

Reservations are durable DigitalOcean tags. Cleanup never refunds them and keeps
recent accounting tags. A lost reservation response prevents resource creation.
A lost Droplet/image creation response is never blindly retried. Resources are
recoverable by their reserved names as well as tags. Do not delete reservation
tags to bypass an exhausted budget. Change the reviewed policy if more capacity
is needed.

DigitalOcean API/Functions outages or revoked cleanup credentials can delay
deletion beyond the reservation allowance. The $20 admission budget cannot stop
billing for a resource the provider will not delete. Powering off does not stop
Droplet billing. Compute reservations also exclude outbound transfer, image
storage, Functions, Spaces and GitHub charges.

Spaces has a $5/month base subscription if you do not already have one. Custom
images cost $0.06/GB-month. Private GitHub-hosted jobs and artifact storage can
incur charges. Configure a GitHub Actions budget with **Stop usage when budget
limit is reached** before enabling CI; $0 paid overage is a reasonable starting
point. That can block GitHub cleanup, so the independent Function is essential.
The hourly GitHub health check alone can use roughly 744 minimum-billed job
minutes in a 31-day month. It is a notification path, not the deletion mechanism.

## Independent deletion and acceptance

The private scheduled DigitalOcean Function deletes expired owned droplets and
cleans up image storage. It publishes a versioned heartbeat only after all
cleanup succeeds. New operations stop when that heartbeat is over five minutes
old or overdue resources remain. The Function needs no GitHub availability.

A heartbeat alone proves neither deletion permissions nor successful deletion.
`PV cleanup canary` creates a small Ubuntu droplet expiring after one minute,
waits for the Function to delete it, and requires the Droplet API to return 404.
Only then does it record a successful canary. Normal test provisioning requires
that proof to be no older than 30 days. GitHub's separate cleanup job removes a
failed canary, but does not mark the independent test as passed.

After the first image build, also test normal completion and a canceled test job,
verify the owned droplet list is empty, inspect artifacts, and measure startup.
Local unit tests and workflow lint do not establish these live properties.

The names `seedbox-pv-job-*`, `seedbox-pv-*` image names, and `seedbox-ci-*` tags
are reserved for this controller. Do not apply them to unrelated resources.
Cloud credentials never enter a test droplet; boot metadata contains only a
single-use GitHub runner configuration.

## Setup

Use dedicated CI credentials, not a managed Robo/Codex token. All cloud secrets
belong to a GitHub Environment named `pv-ci`, restricted to deployment branch
`main`. Create that Environment under repository Settings → Environments and
configure its deployment branch rule before adding secrets. Test workers do not
use that Environment. Leave the repository variable `PV_CI_ENABLED` unset until
the canary succeeds.

1. Create a DigitalOcean CI token with Droplet read/create/delete, image
   read/create/delete, tag read/create/delete and sizes read access. The Function
   token needs Droplet read/delete, image read/delete and tag read/create/delete.
   A separate setup token/context may need Functions administration access.

   ```sh
   robo-gh secret set DO_CI_TOKEN --repo gardnervickers/seedbox --env pv-ci
   ```

2. Create a private, never-versioned Spaces staging bucket (or reuse a suitable
   existing one). DigitalOcean's bucket-scoped read/write keys cannot read the
   lifecycle configuration: live verification returned `AccessDenied`. The
   current storage preflight therefore requires a full-access Spaces key, which
   covers all current and future Spaces buckets in that team. Approve that scope
   explicitly and keep it in the protected `pv-ci` environment; never give it to
   a test runner. Do not bypass the lifecycle check to use a narrower key.

   ```sh
   robo-gh secret set DO_SPACES_ACCESS_KEY_ID --repo gardnervickers/seedbox --env pv-ci
   robo-gh secret set DO_SPACES_SECRET_ACCESS_KEY --repo gardnervickers/seedbox --env pv-ci
   robo-gh variable set DO_CI_SPACES_BUCKET --repo gardnervickers/seedbox --body YOUR_BUCKET_NAME
   robo-gh variable set DO_CI_SPACES_REGION --repo gardnervickers/seedbox --body nyc3
   robo-gh variable set DO_CI_REGION --repo gardnervickers/seedbox --body nyc3
   ```

   For a new dedicated bucket, apply the supplied lifecycle configuration with
   your authenticated AWS CLI. On an existing bucket, merge these rules with its
   existing rules instead of replacing them:

   ```sh
   aws --endpoint-url https://nyc3.digitaloceanspaces.com s3api put-bucket-lifecycle-configuration \
     --bucket YOUR_BUCKET_NAME --lifecycle-configuration file://tools/pv-ci/spaces-lifecycle.json
   ```

3. Create a GitHub App at <https://github.com/settings/apps/new>, disable
   webhooks, grant **Repository Administration: Read and write**, and install
   it only on `gardnervickers/seedbox`. Generate its private key:

   ```sh
   robo-gh variable set PV_RUNNER_APP_ID --repo gardnervickers/seedbox --body YOUR_APP_ID
   robo-gh secret set PV_RUNNER_APP_PRIVATE_KEY --repo gardnervickers/seedbox --env pv-ci < /path/to/app-key.pem
   ```

4. Deploy the Function using an authenticated `doctl` setup context. Supply the
   cleanup token privately to the deployment template (zsh example):

   ```sh
   read -rs 'DIGITALOCEAN_TOKEN?Cleanup token: '; echo
   export DIGITALOCEAN_TOKEN
   doctl serverless install
   doctl serverless namespaces create --label seedbox-ci --region nyc1
   doctl serverless namespaces list
   doctl serverless connect YOUR_NAMESPACE_ID
   doctl serverless deploy tools/pv-ci/watchdog
   unset DIGITALOCEAN_TOKEN
   ```

   After two minutes, verify scheduled activations:

   ```sh
   doctl serverless activations logs --function seedbox/sweep --limit 2
   ```

5. Configure the GitHub Actions spending stop, then, with these files on main,
   run the deletion canary and watch its result:

   ```sh
   robo-gh workflow run pv-canary.yml --repo gardnervickers/seedbox --ref main
   robo-gh run list --repo gardnervickers/seedbox --workflow pv-canary.yml --limit 3
   robo-gh run watch RUN_ID --repo gardnervickers/seedbox --exit-status
   ```

6. Only after the canary passes, enable normal operations and build the image:

   ```sh
   robo-gh variable set PV_CI_ENABLED --repo gardnervickers/seedbox --body true
   robo-gh workflow run pv-image.yml --repo gardnervickers/seedbox --ref main
   robo-gh run list --repo gardnervickers/seedbox --workflow pv-image.yml --limit 3
   robo-gh run watch RUN_ID --repo gardnervickers/seedbox --exit-status
   robo-gh workflow run pv-tests.yml --repo gardnervickers/seedbox --ref main -f test_ref=main
   ```

   Verify normal completion and cancellation cleanup with
   `doctl compute droplet list --tag-name seedbox-ci-pv`, and inspect reserved-name
   droplets too if tags were changed. To stop new normal jobs, set `PV_CI_ENABLED`
   to `false`; leave the Function deployed. Cleanup jobs remain enabled.

## Sources

* [Custom image pricing](https://docs.digitalocean.com/products/custom-images/details/pricing/)
* [Spaces pricing](https://docs.digitalocean.com/products/spaces/details/pricing/)
* [Spaces lifecycle support](https://docs.digitalocean.com/products/spaces/reference/s3-compatibility/)
* [GitHub spending stops](https://docs.github.com/en/billing/how-tos/set-up-budgets)
* [Scheduled Functions](https://docs.digitalocean.com/products/functions/how-to/schedule-functions/)

# Install

**AWS Aurora & RDS MySQL Slow Query Monitor** — everything needed to get it running, in order.

This is the operator document. Design rationale (why the collector works the way it does, the
ADRs, the data model, the cost model) is **not published**: it lives outside the repository, and
comment links to `.claude/docs/…` in the source point at it. You do not need any of it to install
or run this.

Korean: [install.ko.md](install.ko.md).

---

## 0. Two install paths

Sections 1–8 describe the **manual** path: every resource created by hand with the console or CLI.
That is the reference — it shows exactly what the app needs and why each permission is narrowed.

There is also a **Terraform** path. It creates the same resources, split into independent layers
with their own state, and is what the maintainer actually runs.

| | Manual (§1–8) | Terraform (`infra/`) |
|---|---|---|
| When | evaluating, one environment, or no Terraform in your org | repeatable, multiple environments |
| Effort | ~20 resources by hand | `terraform apply` per layer |
| Drift | you own it | state owns it |
| Layers | — | `00-bootstrap` → `10-foundation` → `(20/30/50/60)` → `40-compute` |

Per-layer commands and required variables live in [`infra/README.md`](../infra/README.md).
Two things about that path are worth stating here because they bite:

- **Layer order is not advisory.** `40-compute` reads `10-foundation`'s state object directly, so
  running it first fails at `plan`, not at `apply`.
- **Keep the apply inputs in a file.** Terraform state does not store root-module input variables.
  Lose them and you must reconstruct from live resources — and an apply that forgets
  `db_auth_resource_ids` falls back to `dbuser:*/dbmon`, silently granting database auth to every
  instance in the account. Copy `infra/layers/40-compute/dev.tfvars.example`, fill it in, and
  always pass `-var-file`.

Either way, §3 (IAM) and §4 (networking) are the sections to read carefully — the target
database's security group is **not** managed by this project in either path.

## 1. Image

`.github/workflows/release.yml` publishes on every push to `main`:

```
ghcr.io/<owner>/<repo>:latest          # multi-arch (arm64 + amd64)
ghcr.io/<owner>/<repo>:sha-<commit>    # immutable — use this in task definitions
```

That is where the workflow stops. It carries no AWS credentials — publishing an image and
deploying it are different jobs with different blast radius, and a build runner that can write
to your account is a much bigger target than one that can write to a registry.

### Getting the image into ECS

If the GHCR package is **public**, ECS pulls it with no extra configuration.

If it is **private** (the default for a private repository), the task's *execution* role needs
registry credentials. Store a GitHub personal access token that has `read:packages`:

```bash
aws secretsmanager create-secret --name dbmon/ghcr \
  --secret-string '{"username":"<github-user>","password":"<PAT with read:packages>"}'
```

Reference it in the container definition, and grant `secretsmanager:GetSecretValue` on that ARN
to the **execution** role (not the task role — the app must never be able to pull images):

```jsonc
"repositoryCredentials": {
  "credentialsParameter": "arn:aws:secretsmanager:ap-northeast-2:123456789012:secret:dbmon/ghcr-AbCdEf"
}
```

Or copy the image into ECR once per release. Use `imagetools create`, not a rebuild — it moves
the same digest, so the bytes you tested are the bytes you run:

```bash
docker buildx imagetools create \
  -t 123456789012.dkr.ecr.ap-northeast-2.amazonaws.com/dbmon:sha-<commit> \
  ghcr.io/<owner>/<repo>:sha-<commit>
```

ECR is the usual choice for ECS — same-account IAM, no egress charges, VPC endpoint support —
and it removes the pull-credential problem entirely.

The image is **arm64-first** (Graviton: same performance, ~20% cheaper). It runs as UID 10001,
contains no compiler or shell utilities, and ships a `HEALTHCHECK` that calls the binary's own
`healthcheck` subcommand — so no `curl` in the runtime layer.

Build locally:

```bash
docker build -t dbmon:dev .
docker run --rm dbmon:dev --version
```

## 2. Storage (DynamoDB)

Two tables. **They must exist before the task starts** — the app does not create them
(creating tables needs `dynamodb:CreateTable`, and a monitoring task should not hold that).

| Table | Keys | Notes |
|---|---|---|
| `dbmon-data` | PK `S`, SK `S` | Slow queries, digests, rollups, instances, checkpoints, tuning advice. TTL attribute `ttl` (35 days). |
| `dbmon-config` | PK `S`, SK `S` | Settings (`CFG/GLOBAL`), pause scopes, leases. No TTL. |

`dbmon-data` needs two GSIs:

| Index | PK | SK | Used for |
|---|---|---|---|
| `GSI1` | `GSI1PK` | `GSI1SK` | digest → recent samples, and the sparse in-flight sweep |
| `GSI2` | `GSI2PK` | `GSI2SK` | duration-bucket queries |

Point-in-time recovery on both is advised. `terraform -chdir=infra/layers/10-storage apply`
creates them with the right key schema (provider 6.x renamed `hash_key` inside GSIs — that is
why the lock file is committed).

**There is no S3 offload.** Plans are stored inline on the record. A plan whose masked JSON
exceeds 150 KB is dropped with the reason `too_large:<n>KB` and the rest of the record is kept —
losing the slow query itself because its plan was big would be the worse trade. (An earlier
`storage.plan_bucket` setting was declared but never read, so it is gone; the offload is on the
roadmap.)

### With the CLI

Set the environment first; every command below uses these.

```bash
export AWS_REGION=ap-northeast-2
export ENV=dev
export ACCOUNT=$(aws sts get-caller-identity --query Account --output text)
```

**KMS key** — both tables are encrypted with it.

```bash
KEY_ID=$(aws kms create-key --description "dbmon data encryption ($ENV)" \
  --query KeyMetadata.KeyId --output text)
aws kms create-alias --alias-name "alias/dbmon-data-$ENV" --target-key-id "$KEY_ID"
KEY_ARN=$(aws kms describe-key --key-id "$KEY_ID" --query KeyMetadata.Arn --output text)
echo "$KEY_ARN"
```

**`dbmon-data`** — both GSIs and the TTL in one go.

```bash
aws dynamodb create-table --table-name "dbmon-data-$ENV" \
  --billing-mode PAY_PER_REQUEST \
  --sse-specification "Enabled=true,SSEType=KMS,KMSMasterKeyId=$KEY_ARN" \
  --attribute-definitions \
    AttributeName=PK,AttributeType=S AttributeName=SK,AttributeType=S \
    AttributeName=GSI1PK,AttributeType=S AttributeName=GSI1SK,AttributeType=S \
    AttributeName=GSI2PK,AttributeType=S AttributeName=GSI2SK,AttributeType=S \
  --key-schema AttributeName=PK,KeyType=HASH AttributeName=SK,KeyType=RANGE \
  --global-secondary-indexes '[
    {"IndexName":"GSI1",
     "KeySchema":[{"AttributeName":"GSI1PK","KeyType":"HASH"},
                  {"AttributeName":"GSI1SK","KeyType":"RANGE"}],
     "Projection":{"ProjectionType":"INCLUDE","NonKeyAttributes":[
       "record_id","instance_id","env","started_at_ms","duration_ms","app_digest",
       "statement_type","state","last_seen_at_ms","owner_worker","exec_count",
       "total_time_ms","severity","rule_id","owner_epoch","thread_id","abandoned_reason"]}},
    {"IndexName":"GSI2",
     "KeySchema":[{"AttributeName":"GSI2PK","KeyType":"HASH"},
                  {"AttributeName":"GSI2SK","KeyType":"RANGE"}],
     "Projection":{"ProjectionType":"INCLUDE","NonKeyAttributes":[
       "record_id","instance_id","env","started_at_ms","duration_ms","app_digest",
       "statement_type","schema_name","db_user","kind"]}}]'

aws dynamodb update-time-to-live --table-name "dbmon-data-$ENV" \
  --time-to-live-specification "Enabled=true,AttributeName=ttl"
```

> ⚠ **Do not use `ALL` projection on the GSIs.** `sql_text` and `plan_json` would be copied into
> the indexes, doubling write cost and size. It also **hides a production-only defect**: the orphan
> sweep reads keys from the index and fetches bodies from the base table, and with `ALL` that path
> is never exercised, so tests pass while production fails. (Learned the hard way.)

**`dbmon-config`** — no GSIs.

```bash
aws dynamodb create-table --table-name "dbmon-config-$ENV" \
  --billing-mode PAY_PER_REQUEST \
  --sse-specification "Enabled=true,SSEType=KMS,KMSMasterKeyId=$KEY_ARN" \
  --attribute-definitions AttributeName=PK,AttributeType=S AttributeName=SK,AttributeType=S \
  --key-schema AttributeName=PK,KeyType=HASH AttributeName=SK,KeyType=RANGE

aws dynamodb update-time-to-live --table-name "dbmon-config-$ENV" \
  --time-to-live-specification "Enabled=true,AttributeName=ttl"
```

**PITR and deletion protection** — dropping these tables loses the data.

```bash
for t in "dbmon-data-$ENV" "dbmon-config-$ENV"; do
  aws dynamodb update-continuous-backups --table-name "$t" \
    --point-in-time-recovery-specification PointInTimeRecoveryEnabled=true
  aws dynamodb update-table --table-name "$t" --deletion-protection-enabled
done
```

**Verify** — a wrong key schema makes the app read zero rows silently.

```bash
aws dynamodb describe-table --table-name "dbmon-data-$ENV" \
  --query 'Table.[TableStatus,KeySchema,GlobalSecondaryIndexes[].IndexName]' --output json
```


## 3. IAM — task role

The task role is the only identity the app uses. Nine statements, each narrowed on purpose;
`infra/layers/40-compute/iam.tf` is the authoritative version.

### 3.1 Storage

```json
{ "Sid": "DynamoDbData", "Effect": "Allow",
  "Action": ["dynamodb:GetItem","dynamodb:PutItem","dynamodb:UpdateItem","dynamodb:DeleteItem",
             "dynamodb:Query","dynamodb:BatchWriteItem","dynamodb:BatchGetItem",
             "dynamodb:DescribeTable"],
  "Resource": ["arn:aws:dynamodb:ap-northeast-2:123456789012:table/dbmon-data",
               "arn:aws:dynamodb:ap-northeast-2:123456789012:table/dbmon-data/index/*",
               "arn:aws:dynamodb:ap-northeast-2:123456789012:table/dbmon-config"] }
```

```json
{ "Sid": "DenyScan", "Effect": "Deny", "Action": ["dynamodb:Scan"], "Resource": "*" }
```

`DescribeTable` is there because the readiness probe calls it — without it the storage probe
fails with `AccessDenied` before the service can ever report ready.

The explicit `Deny` on `Scan` is deliberate: a single accidental scan over a 500-instance table
is both a cost event and a latency event. The code never scans; this makes that structural.

### 3.2 Discovery (RDS)

```json
{ "Sid": "DiscoveryReadOnly", "Effect": "Allow",
  "Action": ["rds:DescribeDBInstances","rds:DescribeDBClusters",
             "rds:DescribeDBParameters","rds:DescribeDBParameterGroups",
             "rds:DescribeDBClusterParameters","rds:DescribeEvents",
             "rds:DescribePendingMaintenanceActions","rds:ListTagsForResource"],
  "Resource": "*" }
```

`ListMetrics` is **not** granted: the metric catalog lives in the code
(`dbmon_core::cw_metrics`), so there is nothing to enumerate.

`Resource: "*"` is required — `rds:Describe*` does not support resource-level permissions.
Scope is enforced in configuration instead (`discovery.allowed_vpc_ids`,
`denied_name_substrings`, `reject_production_tags`).

### 3.3 Database connection (IAM auth — no passwords)

```json
{ "Effect": "Allow", "Action": ["rds-db:connect"],
  "Resource": ["arn:aws:rds-db:ap-northeast-2:123456789012:dbuser:db-ABC123…/dbmon",
               "arn:aws:rds-db:ap-northeast-2:123456789012:dbuser:cluster-XYZ789…/dbmon"] }
```

**Enumerate the resource IDs.** `dbuser:*/dbmon` means "the `dbmon` account on *any* instance in
this account" — production included. The Terraform variable `db_auth_resource_ids` falls back to
that wildcard when left empty, so an apply that forgets it silently widens reach.

> The resource ID is **not** the instance name.
>
> ```bash
> # RDS instances → DbiResourceId (db-…)
> aws rds describe-db-instances --query 'DBInstances[].[DBInstanceIdentifier,DbiResourceId]' --output text
> # Aurora → DbClusterResourceId (cluster-…)
> aws rds describe-db-clusters  --query 'DBClusters[].[DBClusterIdentifier,DbClusterResourceId]' --output text
> ```
>
> **Aurora members authenticate against the *cluster* resource ID.** Using a member's
> `DbiResourceId` makes the generated token invalid and the connection fails with `1045
> Access denied` — the same error you get from a wrong password, which is why this costs hours.

### 3.4 Metrics and slow logs

```json
{ "Sid": "PutOwnMetrics", "Effect": "Allow",
  "Action": ["cloudwatch:PutMetricData"], "Resource": "*",
  "Condition": { "StringEquals": { "cloudwatch:namespace": "dbmon" } } }
```

```json
{ "Sid": "MetricsRead", "Effect": "Allow",
  "Action": ["cloudwatch:GetMetricData"], "Resource": "*" }
```

> ⚠ **Do not put a namespace condition on `GetMetricData`.** The `cloudwatch:namespace`
> condition key is supplied for `PutMetricData` but **not** for `GetMetricData`, so a statement
> conditioned on it never matches and the result is `implicitDeny`. An earlier version of this
> README shipped `StringEquals { "cloudwatch:namespace": "AWS/RDS" }` here — following it left the
> CloudWatch columns permanently empty while the UI reported a missing permission. Verify with
> `aws iam simulate-principal-policy` (local development runs as admin and hides this).
>
> Be honest about the blast radius: this role can read **every** CloudWatch metric in the account.
> IAM cannot narrow it; a permissions boundary or SCP can. What the code actually queries is the
> catalog in `dbmon_core::cw_metrics`, and that is `AWS/RDS` only.

```json
{ "Sid": "SlowLogRead", "Effect": "Allow", "Action": ["logs:FilterLogEvents"],
  "Resource": ["arn:aws:logs:ap-northeast-2:123456789012:log-group:/aws/rds/instance/*/slowquery",
               "arn:aws:logs:ap-northeast-2:123456789012:log-group:/aws/rds/instance/*/slowquery:*",
               "arn:aws:logs:ap-northeast-2:123456789012:log-group:/aws/rds/cluster/*/slowquery",
               "arn:aws:logs:ap-northeast-2:123456789012:log-group:/aws/rds/cluster/*/slowquery:*"] }
```

> ⚠ **The `cluster` form is what makes Aurora work.** Aurora writes its slow log to a
> *cluster-level* group (`/aws/rds/cluster/<cluster>/slowquery`) with one **stream per member**,
> while RDS uses `/aws/rds/instance/<instance>/slowquery`. Omit the cluster form and Aurora
> backfill never runs — and the failure is quiet: the instance still collects, but
> `rows_examined` stays empty because the exact metrics only exist in the slow log.

Slow logs contain **query literals**. `/aws/rds/{instance,cluster}/*/slowquery` is far narrower
than `/aws/rds/*`, which would include the audit log — that logs every statement.

If a group does not exist yet (slow log never written, or log export disabled) the app reports
`슬로우로그 원천이 없다 … (장애가 아니다)` once per round at info level and keeps going — a
missing group is an environment fact, not an outage.

### 3.5 Multi-region

No extra IAM. The same task role works in every region; the app creates a client per region.
There is no "all regions" option on purpose: each region costs a `DescribeDBInstances` call per
discovery round, and most accounts have RDS in one or two.

**Two lists, and they are not interchangeable.**

| List | Reloads without restart? | Governs |
|---|---|---|
| **Settings → Discovery scope** | yes, within 30 s | which regions get scanned |
| `aws.target_regions` (file) | no | which regions get an IAM-auth and slow-log client |

The IAM-auth token providers and CloudWatch Logs clients are built **once at startup** from
`aws.target_regions`. A region added only through Settings will therefore be discovered — the
instances appear in the list — but collection reports "no auth provider for this instance" and the
slow-log read has no client. **Put every region you collect from in both places**, and restart
after adding one to the file. CloudWatch metric clients are the exception: they are created lazily
per `(account, region)`, so fleet metrics work for a Settings-only region.

Everything the app reads per region needs to exist there: slow-log groups are regional, and
CloudWatch metrics live in the instance's region (the app keys its CloudWatch clients by
`(account, region)` for exactly this reason).

### 3.6 Multi-account

Management account task role:

```json
{ "Sid": "AssumeDiscoveryRole", "Effect": "Allow", "Action": ["sts:AssumeRole"],
  "Resource": ["arn:aws:iam::111122223333:role/dbmon-discovery"] }
```

Target account role `dbmon-discovery` — trust policy:

```json
{ "Version": "2012-10-17",
  "Statement": [{ "Effect": "Allow",
    "Principal": { "AWS": "arn:aws:iam::123456789012:role/dbmon-task" },
    "Action": "sts:AssumeRole",
    "Condition": { "StringEquals": { "sts:ExternalId": "dbmon" } } }] }
```

Permissions:

```json
{ "Version": "2012-10-17",
  "Statement": [{ "Effect": "Allow",
    "Action": ["rds:DescribeDBInstances","rds:DescribeDBClusters","rds:ListTagsForResource",
               "cloudwatch:GetMetricData"],
    "Resource": "*" }] }
```

**Listing is an API call — it needs no network path.** VPC peering or Transit Gateway is only
required for the *next* step, connecting to the database. So "appears in the list but
`unreachable`" is a normal state, and the UI distinguishes it.

**Cross-account *collection* is not supported yet.** Discovery and CloudWatch metrics assume the
target-account role; the IAM database-auth token and the slow-log client do not — they run on the
management account's own credentials. Both paths now refuse a cross-account instance instead of
guessing:

| Path | Cross-account | If it were not refused |
|---|---|---|
| Discovery (`rds:Describe*`) | **works** (assumes the role) | — |
| CloudWatch metrics | **works** (assumes the role) | — |
| IAM DB auth (`rds-db:connect`) | refused | a management-signed token reaches the target DB and fails as "no DB account" |
| Slow log (`logs:FilterLogEvents`) | refused | the group name carries no account, so a same-named instance in the management account gets read and **its SQL stored against the target instance** |

So a cross-account instance appears in the list, shows metrics, and stays `unreachable` for
collection. Per-account token providers and log clients are on the roadmap.

Enter the account list in **Settings → Discovery scope**; it must match
`var.discovery_account_ids`, otherwise discovery fails with `AccessDenied` and those instances
never appear.

### 3.7 Bedrock (AI tuning)

```json
{ "Sid": "InvokeTuningModel", "Effect": "Allow", "Action": ["bedrock:InvokeModel"],
  "Resource": [
    "arn:aws:bedrock:ap-northeast-2:123456789012:inference-profile/global.anthropic.claude-sonnet-5",
    "arn:aws:bedrock:*::foundation-model/anthropic.claude-sonnet-5"
  ] }
```

Two ARNs are needed for cross-region inference profiles: **the profile and the foundation
model it routes to.** Allowing only the profile fails at call time with `AccessDenied`.

Enumerate models rather than using `*` — an account's model list includes image and video
models whose per-call price is orders of magnitude different.

Check what your account can use:

```bash
aws bedrock list-inference-profiles \
  --query 'inferenceProfileSummaries[?contains(inferenceProfileId,`claude`)].inferenceProfileId'
```

Model must also be enabled in the Bedrock console (model access) for the region.

Measured behaviour worth knowing:

- `temperature` is rejected by Claude 5 models (`ValidationException: temperature is
  deprecated for this model`), so the app sends only `maxTokens`.
- A query containing `SLEEP()` can be blocked by some models (`stop_reason=content_filtered`,
  empty output) because it looks like a time-based SQL injection payload. Opus 5 blocks it;
  Sonnet 5 does not. The UI shows that reason verbatim so you can switch models.
- Opus models are verbose — raise **output token limit** to 8000 if answers get truncated.

### 3.8 Alert channel secret

```json
{ "Sid": "ReadChannelSecret", "Effect": "Allow", "Action": ["secretsmanager:GetSecretValue"],
  "Resource": ["arn:aws:secretsmanager:ap-northeast-2:123456789012:secret:dbmon/channel/*"] }
```

Prefix-scoped, because `*` would let the monitoring task read every secret in the account —
including RDS master passwords.

### 3.9 Execution role (separate from the task role)

The ECS **execution** role pulls the image and writes container logs:
`AmazonECSTaskExecutionRolePolicy`, plus `kms:Decrypt` if your ECR or log group uses a CMK.
Keep it separate from the task role — the app must never be able to pull or push images.

### 3.10 Creating the roles and policies with the CLI

Both roles share the same trust policy.

```bash
cat > /tmp/trust-ecs-tasks.json <<'JSON'
{ "Version": "2012-10-17",
  "Statement": [{ "Effect": "Allow",
    "Principal": { "Service": "ecs-tasks.amazonaws.com" },
    "Action": "sts:AssumeRole" }] }
JSON

aws iam create-role --role-name "dbmon-$ENV-ecs-task" \
  --assume-role-policy-document file:///tmp/trust-ecs-tasks.json
aws iam create-role --role-name "dbmon-$ENV-ecs-execution" \
  --assume-role-policy-document file:///tmp/trust-ecs-tasks.json
```

**Execution role** — the managed policy plus inline KMS/secrets. The managed policy alone cannot
read a CMK-encrypted ECR image or the secret.

```bash
aws iam attach-role-policy --role-name "dbmon-$ENV-ecs-execution" \
  --policy-arn arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy

SECRET_ARN=$(aws secretsmanager describe-secret --secret-id "dbmon/$ENV/auth-token" \
  --query ARN --output text)

cat > /tmp/exec-inline.json <<JSON
{ "Version": "2012-10-17", "Statement": [
  { "Sid": "EcrKmsDecrypt", "Effect": "Allow",
    "Action": ["kms:Decrypt"], "Resource": "$KEY_ARN" },
  { "Sid": "AuthTokenSecret", "Effect": "Allow",
    "Action": ["secretsmanager:GetSecretValue"], "Resource": "$SECRET_ARN" }
]}
JSON
aws iam put-role-policy --role-name "dbmon-$ENV-ecs-execution" \
  --policy-name dbmon-exec-extras --policy-document file:///tmp/exec-inline.json
```

**Task role — storage** (§3.1). Enumerate the table ARNs.

```bash
DATA_ARN="arn:aws:dynamodb:$AWS_REGION:$ACCOUNT:table/dbmon-data-$ENV"
CFG_ARN="arn:aws:dynamodb:$AWS_REGION:$ACCOUNT:table/dbmon-config-$ENV"

cat > /tmp/storage.json <<JSON
{ "Version": "2012-10-17", "Statement": [
  { "Sid": "DynamoDbData", "Effect": "Allow",
    "Action": ["dynamodb:GetItem","dynamodb:BatchGetItem","dynamodb:Query",
               "dynamodb:PutItem","dynamodb:UpdateItem","dynamodb:BatchWriteItem",
               "dynamodb:DeleteItem","dynamodb:DescribeTable"],
    "Resource": ["$DATA_ARN","$DATA_ARN/index/*","$CFG_ARN","$CFG_ARN/index/*"] },
  { "Sid": "DenyScan", "Effect": "Deny", "Action": ["dynamodb:Scan"], "Resource": "*" },
  { "Sid": "KmsUse", "Effect": "Allow",
    "Action": ["kms:Decrypt","kms:GenerateDataKey","kms:DescribeKey"],
    "Resource": "$KEY_ARN",
    "Condition": { "StringLike": { "kms:ViaService": [
      "dynamodb.$AWS_REGION.amazonaws.com" ]}}}
]}
JSON
aws iam put-role-policy --role-name "dbmon-$ENV-ecs-task" \
  --policy-name dbmon-storage --policy-document file:///tmp/storage.json
```

**Task role — discovery, metrics, slow logs** (§3.2, §3.4). Omit the `cluster` form and Aurora
silently never backfills.

```bash
cat > /tmp/discovery.json <<JSON
{ "Version": "2012-10-17", "Statement": [
  { "Sid": "DiscoveryReadOnly", "Effect": "Allow",
    "Action": ["rds:DescribeDBInstances","rds:DescribeDBClusters",
               "rds:DescribeDBParameters","rds:DescribeDBParameterGroups",
               "rds:DescribeDBClusterParameters","rds:DescribeEvents",
               "rds:DescribePendingMaintenanceActions","rds:ListTagsForResource"],
    "Resource": "*" },
  { "Sid": "PutOwnMetrics", "Effect": "Allow",
    "Action": ["cloudwatch:PutMetricData"], "Resource": "*",
    "Condition": { "StringEquals": { "cloudwatch:namespace": "dbmon" }}},
  { "Sid": "MetricsRead", "Effect": "Allow",
    "Action": ["cloudwatch:GetMetricData"], "Resource": "*" },
  { "Sid": "SlowLogRead", "Effect": "Allow", "Action": ["logs:FilterLogEvents"],
    "Resource": [
      "arn:aws:logs:$AWS_REGION:$ACCOUNT:log-group:/aws/rds/instance/*/slowquery",
      "arn:aws:logs:$AWS_REGION:$ACCOUNT:log-group:/aws/rds/instance/*/slowquery:*",
      "arn:aws:logs:$AWS_REGION:$ACCOUNT:log-group:/aws/rds/cluster/*/slowquery",
      "arn:aws:logs:$AWS_REGION:$ACCOUNT:log-group:/aws/rds/cluster/*/slowquery:*"] }
]}
JSON
aws iam put-role-policy --role-name "dbmon-$ENV-ecs-task" \
  --policy-name dbmon-discovery --policy-document file:///tmp/discovery.json
```

**Task role — IAM database auth** (§3.3). Look the resource IDs up first.

```bash
# collect resource IDs for RDS instances and Aurora clusters (use only the ones you need)
aws rds describe-db-instances \
  --query 'DBInstances[].[DBInstanceIdentifier,DbiResourceId,DBClusterIdentifier]' --output table
aws rds describe-db-clusters \
  --query 'DBClusters[].[DBClusterIdentifier,DbClusterResourceId]' --output table

# put the IDs you picked here. **Aurora uses the cluster- one.**
IDS=(db-ABC123EXAMPLE cluster-XYZ789EXAMPLE)
RES=$(printf '"arn:aws:rds-db:%s:%s:dbuser:%s/dbmon",' "$AWS_REGION" "$ACCOUNT" "${IDS[@]}")
cat > /tmp/dbauth.json <<JSON
{ "Version": "2012-10-17", "Statement": [
  { "Effect": "Allow", "Action": ["rds-db:connect"], "Resource": [${RES%,}] }]}
JSON
aws iam put-role-policy --role-name "dbmon-$ENV-ecs-task" \
  --policy-name dbmon-db-auth --policy-document file:///tmp/dbauth.json
```

**Recommended — block self-escalation.** `ForAllValues` here is bypassable (same class of trap as
the warning in §3.1).

```bash
cat > /tmp/deny-authz.json <<JSON
{ "Version": "2012-10-17", "Statement": [
  { "Sid": "DenyUserRecordWrites", "Effect": "Deny",
    "Action": ["dynamodb:PutItem","dynamodb:UpdateItem",
               "dynamodb:DeleteItem","dynamodb:BatchWriteItem"],
    "Resource": ["$CFG_ARN"],
    "Condition": { "ForAnyValue:StringLike": { "dynamodb:LeadingKeys": ["USER#*"] }}},
  { "Sid": "DenyAuditMutation", "Effect": "Deny",
    "Action": ["dynamodb:UpdateItem","dynamodb:DeleteItem","dynamodb:BatchWriteItem"],
    "Resource": ["$CFG_ARN"],
    "Condition": { "ForAnyValue:StringLike": { "dynamodb:LeadingKeys": ["AUDIT#*"] }}}
]}
JSON
aws iam put-role-policy --role-name "dbmon-$ENV-ecs-task" \
  --policy-name dbmon-deny-authz-writes --policy-document file:///tmp/deny-authz.json
```

**Always verify with the simulator afterwards.** Local development runs as administrator, which
hides condition-key mistakes — that is how the `GetMetricData` trap in §3.4 reached production.

```bash
ROLE="arn:aws:iam::$ACCOUNT:role/dbmon-$ENV-ecs-task"

# must be allowed
aws iam simulate-principal-policy --policy-source-arn "$ROLE" \
  --action-names cloudwatch:GetMetricData rds:DescribeDBInstances logs:FilterLogEvents \
  --query 'EvaluationResults[].[EvalActionName,EvalDecision]' --output text

# must be denied — expect explicitDeny
aws iam simulate-principal-policy --policy-source-arn "$ROLE" \
  --action-names dynamodb:Scan --resource-arns "$DATA_ARN" \
  --query 'EvaluationResults[0].EvalDecision' --output text

# is self-escalation blocked? a mixed batch must be denied too
aws iam simulate-principal-policy --policy-source-arn "$ROLE" \
  --action-names dynamodb:BatchWriteItem --resource-arns "$CFG_ARN" \
  --context-entries 'ContextKeyName=dynamodb:LeadingKeys,ContextKeyType=stringList,ContextKeyValues=USER#me,CFG' \
  --query 'EvaluationResults[0].EvalDecision' --output text
```


## 4. Networking

| Direction | Rule |
|---|---|
| Task → RDS | DB security group: allow **TCP 3306 from the task security group** (not a CIDR). Security-group references survive IP changes. |
| Task → AWS APIs | NAT gateway, or interface VPC endpoints for `dynamodb` (gateway), `rds`, `monitoring`, `logs`, `secretsmanager`, `bedrock-runtime`, `sts`, `ecr.api`, `ecr.dkr`, plus the **S3 gateway endpoint** (ECR stores image layers in S3 — the app itself never touches S3). Endpoints avoid NAT data charges and keep traffic off the internet. |
| ALB → Task | Target group on 8080, health check path **`/readyz`**. |
| Task inbound | The ALB security group, **or** an admin/VPN security group when running without an ALB. Nothing else. |

**Running without an ALB is supported and is the cheaper default.** An ALB bills per hour; for a
VPN-reachable internal tool you can open the container port to the VPN security group instead:

```bash
aws ec2 authorize-security-group-ingress --group-id <task-sg> \
  --ip-permissions "IpProtocol=tcp,FromPort=8080,ToPort=8080,\
UserIdGroupPairs=[{GroupId=<vpn-sg>,Description='from VPN to container port'}]"

# the task's private IP
TASK=$(aws ecs list-tasks --cluster dbmon --service-name dbmon --query 'taskArns[0]' --output text)
aws ecs describe-tasks --cluster dbmon --tasks "$TASK" \
  --query 'tasks[0].attachments[0].details[?name==`privateIPv4Address`].value' --output text
```

> ⚠ **The two health checks are not interchangeable.** The *container* health check must use
> `/healthz` and the *target group* must use `/readyz`. A standby worker answers `/readyz` with 503
> while being perfectly alive (F1) — point the container check at `/readyz` and ECS kills and
> restarts standby workers forever. Point the target group at `/healthz` and traffic goes to a
> worker that is not ready.

**ALB idle timeout must be ≥ 300 seconds.** The WebSocket carries live metrics, and the AI
tuning request can run ~30–100 seconds. At the default 60 s the UI reconnects constantly and
the tuning button returns 504 — which says nothing about the cause.

Aurora: the app connects to the **writer** endpoint of each instance it discovers (instance
endpoints, not the cluster endpoint) so `performance_schema` reads are attributed correctly.

### Creating the security groups with the CLI

```bash
VPC=vpc-0123456789abcdef0

SG=$(aws ec2 create-security-group --group-name "dbmon-$ENV-task" \
  --description "dbmon ECS tasks" --vpc-id "$VPC" --query GroupId --output text)
echo "$SG"
```

**Egress is open by default** — leave it: the task has to reach the target RDS, the AWS APIs and the
notification channels. AWS only allows ASCII in a security group description.

**Ingress — the admin path only.** Without an ALB:

```bash
# from a VPN security group
aws ec2 authorize-security-group-ingress --group-id "$SG" \
  --ip-permissions "IpProtocol=tcp,FromPort=8080,ToPort=8080,\
UserIdGroupPairs=[{GroupId=sg-vpn0123456789,Description='from VPN to container port'}]"

# or from an admin CIDR
aws ec2 authorize-security-group-ingress --group-id "$SG" \
  --ip-permissions "IpProtocol=tcp,FromPort=8080,ToPort=8080,\
IpRanges=[{CidrIp=10.99.0.0/16,Description='from admin CIDR'}]"
```

**Target database ingress — that resource is not ours.** Agree it with the owning team. Use a
**security-group reference, not a CIDR**: task IPs change on every deployment, so a CIDR is either
too wide or breaks each time.

```bash
# find the target instance's security group
aws rds describe-db-instances --db-instance-identifier <target-instance> \
  --query 'DBInstances[0].VpcSecurityGroups[].VpcSecurityGroupId' --output text

aws ec2 authorize-security-group-ingress --group-id <target-db-sg> \
  --ip-permissions "IpProtocol=tcp,FromPort=3306,ToPort=3306,\
UserIdGroupPairs=[{GroupId=$SG,Description='from dbmon collector'}]"
```

**Verify** the rules actually landed.

```bash
aws ec2 describe-security-groups --group-ids "$SG" \
  --query 'SecurityGroups[0].IpPermissions' --output json
```


## 5. Task definition

```jsonc
{
  "family": "dbmon",
  "cpu": "512", "memory": "1024",
  "requiresCompatibilities": ["FARGATE"],
  "networkMode": "awsvpc",
  "runtimePlatform": { "cpuArchitecture": "ARM64", "operatingSystemFamily": "LINUX" },
  "taskRoleArn": "arn:aws:iam::123456789012:role/dbmon-task",
  "executionRoleArn": "arn:aws:iam::123456789012:role/dbmon-exec",
  "containerDefinitions": [{
    "name": "dbmon",
    "image": "123456789012.dkr.ecr.ap-northeast-2.amazonaws.com/dbmon:sha-<commit>",
    "portMappings": [{ "containerPort": 8080, "protocol": "tcp" }],
    "environment": [
      { "name": "DBMON__DEPLOYMENT_ENV",           "value": "prd" },
      { "name": "DBMON__AWS__REGION",              "value": "ap-northeast-2" },
      { "name": "DBMON__AWS__ACCOUNT_ID",          "value": "123456789012" },
      { "name": "DBMON__STORAGE__DATA_TABLE",      "value": "dbmon-data" },
      { "name": "DBMON__STORAGE__CONFIG_TABLE",    "value": "dbmon-config" },
      { "name": "DBMON__COLLECTOR__MONITOR_DB_USER", "value": "dbmon" },
      { "name": "DBMON__COLLECTOR__LITERAL_POLICY",  "value": "masked" },
      { "name": "DBMON__DISCOVERY__ALLOWED_VPC_IDS", "value": "vpc-0123456789abcdef0" }
    ],
    "secrets": [
      { "name": "DBMON__HTTP__AUTH_TOKEN",
        "valueFrom": "arn:aws:secretsmanager:ap-northeast-2:123456789012:secret:dbmon/api-token" }
    ],
    "stopTimeout": 60,
    "logConfiguration": {
      "logDriver": "awslogs",
      "options": { "awslogs-group": "/ecs/dbmon", "awslogs-region": "ap-northeast-2",
                   "awslogs-stream-prefix": "dbmon" }
    }
  }]
}
```

`stopTimeout` must exceed `http.shutdown_grace_secs` (default 45) or SIGKILL arrives first and
in-flight records are lost.

### Configuration reference

Environment variables override the TOML file; `__` separates levels
(`DBMON__COLLECTOR__SLOW_THRESHOLD_SECS`). Startup **fails fast** on invalid config — a
half-working process that collects but cannot store is worse than one that refuses to boot.

**Required**

| Key | Meaning |
|---|---|
| `deployment_env` | `dev` / `stg` / `prd`. In `dev`, discovery filters become mandatory. |
| `aws.region` | Deployment region. |
| `aws.account_id` | Goes into every `instance_id` — **cannot be changed later**. |
| `storage.data_table`, `storage.config_table` | DynamoDB table names. |

**Frequently set**

| Key | Default | Meaning |
|---|---|---|
| `role` | `all` | `all` / `api` / `collector` / `control`. Split when you scale out. |
| `http.bind`, `http.port` | `0.0.0.0`, `8080` | |
| `http.shutdown_grace_secs` | `45` | Must be < ECS `stopTimeout`. |
| `http.deregistration_wait_secs` | `20` | Set `0` without a load balancer. |
| `http.allow_auth_disable` | `false` | Permits the "no authentication" setting. Two opt-ins required. |
| `http.auth_token` | none | **Shared bearer token.** Without it (and without `off`), every request 401s. ≥32 chars, no whitespace, or startup fails. |
| `aws.target_regions` | `[region]` | Fallback when Settings has no region list. |
| `collector.slow_threshold_secs` | `2` | What counts as slow for detection. |
| `collector.monitor_db_user` | `dbmon` | Also the self-exclusion key. |
| `collector.literal_policy` | `masked` | `masked` / `full` / `full_restricted` / `off`. See below. |
| `collector.backfill_secs` | `60` | Slow-log backfill period. |
| `discovery.allowed_vpc_ids` | `[]` | **Required in every non-`prd` environment** (`dev`, `stg`, unknown) — startup validation rejects an empty list. |
| `discovery.collect_production_targets` | `false` | Gate for `prd`-tagged instances. |

**Literal policy** decides whether stored SQL keeps its values:

| Value | Stored SQL | Copy-and-run sample? |
|---|---|---|
| `masked` (default) | `WHERE id = ?` | No |
| `full_restricted` | original | Yes, visible to `operator`+ |
| `full` | original | Yes, subject to `can_see_literals` |
| `off` | not stored | No |

Masking is **one-way**: switching to `masked` later does not remove literals already stored,
and switching away does not recover masked ones. It applies to newly stored records only.
Execution-plan JSON is masked regardless of this setting.

> ⚠ **The runtime image has no config file.** It ships the binary and `web/dist` only, so anything
> not in `environment` falls back to the code default. `local/dbmon-aws.toml` applies to local runs
> only — a value you set there and not here is silently ignored in the deployment.
>
> `DBMON__COLLECTOR__LITERAL_POLICY` is the one that bites: the default `masked` replaces SQL
> literals with `?` **irreversibly**. Set `full_restricted` if you need the original text
> (operator role + audit log on view).
>
> Do not pass keys the app does not know — configuration is `deny_unknown_fields`, so an extra key
> **fails startup** rather than being ignored.

### Deploying with the CLI

**ECR, secret, log group, cluster** — created once.

```bash
aws ecr create-repository --repository-name "dbmon-$ENV" \
  --image-tag-mutability IMMUTABLE \
  --image-scanning-configuration scanOnPush=true \
  --encryption-configuration "encryptionType=KMS,kmsKey=$KEY_ARN"

# needed unless you use Cognito
aws secretsmanager create-secret --name "dbmon/$ENV/auth-token" \
  --secret-string "$(openssl rand -hex 32)"

aws logs create-log-group --log-group-name "/dbmon/$ENV/app"
aws logs put-retention-policy --log-group-name "/dbmon/$ENV/app" --retention-in-days 30

aws ecs create-cluster --cluster-name "dbmon-$ENV"
```

**Image** — the task is Graviton, so `--platform linux/arm64` is required.

```bash
# `Cargo.toml` is the source of truth for the version — typing it by hand drifts from `--version`.
TAG="v$(awk '/^\[workspace\.package\]/{f=1;next} /^\[/{f=0} f && /^version *=/{gsub(/[" ]/,"");sub(/version=/,"");print;exit}' Cargo.toml)-$(git rev-parse --short HEAD)"     # never `latest`
REPO="$ACCOUNT.dkr.ecr.$AWS_REGION.amazonaws.com/dbmon-$ENV"

aws ecr get-login-password --region "$AWS_REGION" \
  | docker login --username AWS --password-stdin "${REPO%/*}"
docker build --platform linux/arm64 -t "$REPO:$TAG" .
docker push "$REPO:$TAG"
```

**Register the task definition** once `taskdef.json` above is filled in.

```bash
aws ecs register-task-definition --cli-input-json "file://taskdef.json"
```

**Create the service** (first time). Spot cuts the bill substantially and this workload tolerates
interruption.

```bash
aws ecs create-service --cluster "dbmon-$ENV" --service-name "dbmon-$ENV" \
  --task-definition "dbmon-$ENV" --desired-count 1 \
  --capacity-provider-strategy capacityProvider=FARGATE_SPOT,weight=1 \
  --network-configuration "awsvpcConfiguration={subnets=[subnet-aaa,subnet-bbb],\
securityGroups=[$SG],assignPublicIp=ENABLED}" \
  --health-check-grace-period-seconds 60
```

`assignPublicIp=ENABLED` with public subnets egresses through the IGW, so there is no NAT charge.
Private subnets need a NAT gateway or interface endpoints.

**Subsequent deployments** (new image only).

```bash
aws ecs register-task-definition --cli-input-json "file://taskdef.json"   # with the new tag
aws ecs update-service --cluster "dbmon-$ENV" --service "dbmon-$ENV" \
  --task-definition "dbmon-$ENV" --force-new-deployment
```

**Check the rollout** — `aws ecs wait services-stable` tells you nothing about *why* something
failed, so look directly.

```bash
aws ecs describe-services --cluster "dbmon-$ENV" --services "dbmon-$ENV" \
  --query 'services[0].{td:taskDefinition,running:runningCount,desired:desiredCount,
                        rollout:deployments[0].rolloutState}' --output json

TASK=$(aws ecs list-tasks --cluster "dbmon-$ENV" --service-name "dbmon-$ENV" \
  --query 'taskArns[0]' --output text)
aws ecs describe-tasks --cluster "dbmon-$ENV" --tasks "$TASK" \
  --query 'tasks[0].{status:lastStatus,health:healthStatus,image:containers[0].image,
                     stopped:stoppedReason}' --output json
```

**Rollback** to a previous revision. Image tags are immutable, so the revision *is* the image.

```bash
aws ecs update-service --cluster "dbmon-$ENV" --service "dbmon-$ENV" \
  --task-definition "dbmon-$ENV:<previous-revision>"
```


## 6. Database account (per instance)

No passwords are stored anywhere. The app authenticates with a 15-minute IAM token, so the
database account must use `AWSAuthenticationPlugin`.

Prerequisites on the instance:

1. **IAM DB authentication enabled** —
   `aws rds modify-db-instance --db-instance-identifier X --enable-iam-database-authentication`
   (Aurora: on the cluster).
2. **Slow query log on, exported to CloudWatch Logs** — parameter group
   `slow_query_log=1`, `long_query_time=1` (or your threshold), `log_output=FILE`, and
   `EnableCloudwatchLogsExports=["slowquery"]`.
3. `performance_schema=1` (default on `db.t3.medium` and larger).

Then, connected as the master user:

```sql
-- The host pattern must match where the task connects FROM.
-- Use the ECS subnet CIDR, not the task IP: tasks get new IPs on every deploy.
CREATE USER IF NOT EXISTS 'dbmon'@'10.1.%'
  IDENTIFIED WITH AWSAuthenticationPlugin AS 'RDS'
  REQUIRE SSL;

-- PROCESS: see other sessions in `information_schema.PROCESSLIST` — without it the
--   account sees only its own threads and detection finds nothing.
-- SHOW VIEW: `SHOW CREATE TABLE` on a view needs it, and plans do reference views.
GRANT PROCESS, SHOW VIEW ON *.* TO 'dbmon'@'10.1.%';

-- Metrics and digests.
GRANT SELECT ON `performance_schema`.* TO 'dbmon'@'10.1.%';

-- Plans and tuning context need SELECT on the monitored schemas.
-- Least privilege: enumerate them. Broad: GRANT SELECT ON *.* (gives cardinality for every table).
GRANT SELECT ON `shop`.* TO 'dbmon'@'10.1.%';

SHOW GRANTS FOR 'dbmon'@'10.1.%';
```

`REPLICATION CLIENT`, `SHOW DATABASES`, and `SELECT ON sys.*` used to be in this list.
The collector never issues a query that needs them — it reads
`performance_schema.{processlist,events_statements_current,events_statements_summary_by_digest,
global_status,global_variables}`, `information_schema.{PROCESSLIST,STATISTICS,TABLES}`,
and `SHOW CREATE TABLE`. A monitoring account that gets compromised should see as little
as the job allows.

Notes that cost time if you miss them:

- **Read replicas (`-ro`)**: create the user on the **source**. A replica is read-only, so
  `CREATE USER` fails there; the account arrives by replication.
- **Aurora**: create on the writer; it propagates to readers.
- **`REQUIRE SSL`** is not optional for IAM auth — the token travels as the password.
- The IAM token is signed for the **real endpoint**. If you tunnel through SSM for local
  development, sign for the endpoint and only redirect the TCP address
  (`collector.target_endpoint_overrides`, dev + loopback only).
- Client VPN performs source NAT: the database sees the *subnet ENI* address, not the VPN
  client CIDR. Grant the subnet pattern (e.g. `10.1.%`), which then covers both ECS tasks and
  your laptop.

`.claude/docs/22-onboarding-a-db.md` walks the whole path, including privilege modes and verification.

## 7. Screen settings

After the service is healthy, open the UI and finish in **Options** (gear, top right):

| Section | What to set |
|---|---|
| **Discovery scope** | Regions to scan; accounts (12-digit ID + role *name*, not ARN) when management-account mode is on. |
| **Login** | `token` / `cognito` (settings only for now) / `off` (requires `http.allow_auth_disable`). **The token itself is not entered here** — see below. |
| **AI tuning** | Enable, model ID, Bedrock region, output token limit. |
| **Notifications** | Slack mode, channel, Secrets Manager **reference**, message template with preview. |

Settings are stored in DynamoDB (`CFG/GLOBAL`) and applied within 30 seconds across all
workers, with optimistic locking so two administrators cannot silently overwrite each other.
Details and failure semantics: [`.claude/docs/23-settings.md`](../.claude/docs/23-settings.md).

Then go to the **RDS** tab and press **Start collection** on each instance. Registration is
automatic; starting is not — collection begins querying the target database every second, and
that should be a human decision.

### Authentication, concretely

There is no "log in with a password" screen. Four ways in, and which one applies is derived from
the deployment — `GET /api/auth/config` reports it, and the UI's notice text follows.

| Mode | Credential | When |
|---|---|---|
| `local-dev` | none | `dev` **and** bound to loopback. Both, so a `dev` deployment on `0.0.0.0` does not get a free pass. |
| `local-token` | random token printed to the startup log | `dev`, non-loopback, **not** on ECS. For `docker run`, where loopback binding is impossible. |
| `shared-token` | `http.auth_token` from the deployment config | **This is the ECS path.** |
| `off` | none — everyone is admin | `http.allow_auth_disable = true` **and** settings `auth.mode = off`. |

`shared-token` is what a real deployment uses:

```bash
openssl rand -hex 32
```

Inject it as `DBMON__HTTP__AUTH_TOKEN` via the task definition's `secrets:` block (§5), so the
definition holds only a Secrets Manager ARN. Shorter than 32 characters or containing whitespace
and **startup fails** — a weak token is worse than none, because it looks like authentication;
whitespace gets cut at the header and produces "the token is right but I get 401".

Open the UI, and the notice asks for the token — **paste it into the field.** It goes to
`sessionStorage`, and pasting makes no HTTP request.

`https://<alb>/?token=<token>` also works but is the wrong choice for a deployment: the token is
stripped from the address bar only *after* the first request has gone out, so the ALB access log
already recorded it. That log then holds a non-expiring admin credential. The URL form stays for
the `dev` container case, where the log is local.

**This token is admin.** One token has no subject, so there is nothing to base a role split on —
whoever holds it can change settings and even turn authentication off. Audit logs record
`subject=shared-token`, distinct from `local-dev` and `anonymous`. Per-person roles are what
Cognito is for.

**With no token and no `off`, every API request returns 401.** The startup log warns about
exactly this, because a bare `401` cannot tell you that one config line is missing.

### Cognito: what is and is not there

The settings screen stores the pool ID, client ID, region, and hosted-UI domain — all public
values a browser must know before login, which is why `/api/auth/config` serves them without
authentication. Do not use an app client with a secret; an SPA cannot keep one.

**The verifier is not wired** (`COGNITO_READY = false` in `crates/dbmon/src/api/auth.rs`).
Selecting `cognito` therefore falls back to the shared-token path: a deployment with a token keeps
working, one without it returns 401. Nothing is stubbed through — a stub would look like
authentication while being none.

Still missing, in order: JWKS fetch from
`https://cognito-idp.<region>.amazonaws.com/<pool>/.well-known/jwks.json`, a `kid`-keyed key cache,
RS256 signature plus `iss`/`aud`/`exp`/`token_use` checks, and mapping claims to an `AuthContext`
via `AuthContext::intersect` so a group becomes a role. `CognitoSettings` already carries every
field those steps need, so this is code, not configuration.

## 8. Verify

```bash
ALB=http://<alb>
# Everything under /api needs the token. /healthz and /readyz do not — they are two of the
# three routes that answer without authentication (the third is /api/auth/config).
AUTH="Authorization: Bearer $DBMON_TOKEN"

# Health
curl -s $ALB/healthz && curl -s $ALB/readyz

# Which authentication mode is actually in effect
curl -s $ALB/api/auth/config | jq '.mode'

# Discovery found the instances
curl -s -H "$AUTH" $ALB/api/instances | jq 'length, .[0].state'

# Fleet metrics (failed_scopes must be empty)
curl -s -H "$AUTH" $ALB/api/metrics/fleet | jq '.failed_scopes, (.rows | length)'

# Collector is leading and ticking
curl -s -H "$AUTH" $ALB/api/collector/status | jq '{is_leader, collecting, last_tick_ms}'
```

If `.mode` is `unconfigured`, stop here — no credential is configured and every `/api` call
returns 401. Set `http.auth_token` (§5) and redeploy.

Common first failures:

| Symptom | Cause |
|---|---|
| Instances stay `pending` | Nobody pressed **Start collection** (by design). |
| `unreachable` | Security group, or the `dbmon` account / IAM auth is missing on that instance. |
| Metrics columns all `—` with `failed_scopes` set | CloudWatch permission, or a cross-account role without `cloudwatch:GetMetricData`. |
| Empty slow query list but instances are `collecting` | Slow log not exported to CloudWatch Logs, or `long_query_time` above your traffic. |
| Tuning button says `model_failed` | The reason is printed next to the button — wrong model ID, model not enabled in the region, or a content filter. |

### What the health signals do and do not cover

| Signal | Meaning | Fails when |
|---|---|---|
| `GET /healthz` | the process answers HTTP | never — it always returns 200 |
| `GET /readyz` | this worker should receive traffic | draining, config not loaded, storage unreachable, KMS denied, or (for a combined api+collector worker) not the collect leader |
| `readyz.collect_stale` | **collection has stopped** | no successful tick for 5 minutes on a worker that should be collecting |
| `GET /api/collector/status` | why | per-instance detail |

ECS's container health check uses `/healthz` **on purpose**: a standby worker answers `/readyz`
with 503 by design (FR-OPS-08 active/standby), so pointing the container check at `/readyz` would
make ECS kill and restart every standby forever. The ALB target group is what uses `/readyz`.

**`collect_stale` does not drop `ready`.** If it did, the load balancer would remove the task and
ECS would replace it — but collection failures are usually environmental (target DB unreachable,
IAM), so replacement does not help and you get an endless replace loop. That is worse than the
outage it is reacting to. So the condition is exposed as a value and alarming is left outside.

**Nothing publishes CloudWatch metrics yet.** `PutMetricData` appears in the IAM policy but no
code calls it, so there is no CloudWatch alarm on `collect_stale` — poll `/readyz` from whatever
you already use for synthetic checks. Publishing FR-OPS-09 metrics is on the roadmap.

## 9. When it does not work

Every row below was hit for real during deployment.

| Symptom | Cause | Where to look |
|---|---|---|
| Task exits immediately | x86 image (the task is Graviton/arm64) | task stopped reason |
| Exits on startup with a config error | an env key the app does not know (`deny_unknown_fields`) | first log line |
| Startup refused in a non-prd environment | `DBMON__DISCOVERY__ALLOWED_VPC_IDS` is empty (T-37) | log |
| Every request is 401 | the shared token was never injected | `GET /api/auth/config` → `mode` |
| SQL is always `?` | `LITERAL_POLICY` not set, so the code default `masked` applies | a record's `literal_policy` |
| CloudWatch columns are all `—` | a namespace condition on `GetMetricData` (§3.4) | `aws iam simulate-principal-policy` |
| Only Aurora has empty `rows_examined` | the slow-log IAM resource list has no `cluster` form (§3.4) | backfill log line, `no_source_instances` |
| Tuning returns `AccessDenied` | `bedrock:InvokeModel` missing, or that model is not enumerated | the error text shown on screen |
| IAM database auth fails with `1045` | an Aurora member's `DbiResourceId` was used instead of the cluster's (§3.3) | the `dbuser:` values in the policy |
| Target database connection times out | the target DB security group has no inbound from the task security group | §4 |
| Standby workers restart forever | the *container* health check points at `/readyz` | §4 |
| Cursor pagination stops after one page | an old build; the cursor's filter hash included the resolved time window | `next_cursor` in the response |

Two diagnostics are worth knowing:

```bash
curl -s "$URL/readyz" | jq       # config_loaded / storage_ok / kms_denied / auth_mode_supported
aws iam simulate-principal-policy --policy-source-arn <task-role-arn> \
  --action-names cloudwatch:GetMetricData rds:DescribeDBInstances logs:FilterLogEvents \
  --query 'EvaluationResults[].[EvalActionName,EvalDecision]' --output text
```

**The simulator is sometimes the only way to see a permission problem**, because local development
runs with administrator credentials and hides condition-key mistakes.

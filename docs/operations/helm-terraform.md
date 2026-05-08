# Helm chart and Terraform module

Two infrastructure-as-code surfaces ship in `deploy/` for teams that
already run Kubernetes or AWS:

- **Helm chart** at [`deploy/helm/branchwork/`](../../deploy/helm/branchwork)
  — installs `branchwork-server` into any Kubernetes cluster as a
  single Deployment, optionally fronted by an Ingress and backed by a
  PVC.
- **Terraform module** at [`deploy/terraform/`](../../deploy/terraform)
  — provisions an AWS ECS Fargate service (ALB + EFS + CloudWatch)
  that runs the same image.

Both wrap the same `ghcr.io/branchwork/branchwork` image documented in
[architecture/deploy.md](../architecture/deploy.md) and the same flag
surface documented in [reference/cli.md](../reference/cli.md). Day-2
ops for plain processes are in
[operations/self-hosted.md](self-hosted.md); compose-based setups are
in [operations/docker.md](docker.md). This page only covers the
chart and the module — every value, every variable, every output.

> **Scope.** Branchwork's auto-mode loop spawns local agents, which
> need a writable repo on the same host as the process. Helm and ECS
> Fargate are great for the dashboard surface (web UI, REST API,
> WebSocket fan-out, plan storage), but agents themselves are best
> hosted out-of-cluster on a runner — see
> [operations/saas-runner.md](saas-runner.md). The chart and the
> Terraform module deploy the **server** only.

## Helm chart

Single chart, one Deployment, optional Ingress and PVC. `Chart.yaml`
declares `version: 0.1.0`, `appVersion: 0.3.0` — the image tag
defaults to `appVersion` when `image.tag` is empty.

### Install

```sh
# From a working copy of the repo:
helm install branchwork ./deploy/helm/branchwork \
  --namespace branchwork --create-namespace

# Override values:
helm install branchwork ./deploy/helm/branchwork \
  -f my-values.yaml

# Upgrade in place:
helm upgrade branchwork ./deploy/helm/branchwork -f my-values.yaml
```

The chart is **not** published to a registry today; install from the
checkout or vendor it into your own chart repo.

### Values

Every key in [`values.yaml`](../../deploy/helm/branchwork/values.yaml).
Defaults match the file verbatim.

#### Image and replicas

| Key | Type | Default | Purpose |
|---|---|---|---|
| `replicaCount` | int | `1` | Pod count. Keep at `1` for SQLite (Deployment uses `Recreate` strategy under `database.mode: sqlite`); scale only with Postgres — see the caveat below. |
| `image.repository` | string | `ghcr.io/branchwork/branchwork` | Container image. |
| `image.tag` | string | `""` | Image tag. Empty falls back to `Chart.appVersion`. |
| `image.pullPolicy` | string | `IfNotPresent` | Standard k8s pull policy. |
| `imagePullSecrets` | list | `[]` | Pull-secret references for private registries. |
| `nameOverride` | string | `""` | Overrides chart `name` in resource names. |
| `fullnameOverride` | string | `""` | Overrides full release name. |

#### Server config (CLI flags)

`branchwork-server` is invoked with these flags (see
[reference/cli.md](../reference/cli.md)). The `--claude-dir` flag is
hard-coded to `/data` inside the pod.

| Key | Type | Default | Purpose |
|---|---|---|---|
| `config.port` | int | `3100` | HTTP port inside the container; passed as `--port`. |
| `config.effort` | string | `high` | Agent effort level: `low` \| `medium` \| `high` \| `max`; passed as `--effort`. |
| `config.webhookUrl` | string | `""` | Slack-shaped webhook URL for agent / plan notifications; exported as `BRANCHWORK_WEBHOOK_URL`. Empty disables the env-var entirely. |

#### Database

| Key | Type | Default | Purpose |
|---|---|---|---|
| `database.mode` | string | `sqlite` | `sqlite` (PVC-backed) or `postgres` (sets `DATABASE_URL`). **Caveat below — `postgres` is a deployment-template stub today.** |
| `database.postgresUrl` | string | `""` | Connection string for `mode: postgres`. Format: `postgres://user:pass@host:5432/branchwork`. |
| `database.existingSecret` | string | `""` | Name of an existing `Secret` with key `DATABASE_URL`. Takes precedence over `postgresUrl`. |

> **`postgres` mode is not yet implemented in the binary.** The chart
> wires `DATABASE_URL` into the pod and skips the PVC, but
> `db.rs` only speaks SQLite — there is no Postgres driver linked.
> Stay on `mode: sqlite` until that lands. Background:
> [architecture/persistence.md § Postgres mode](../architecture/persistence.md#postgres-mode).

#### Persistence (SQLite mode only)

The PVC template is gated on `persistence.enabled && mode == sqlite`.

| Key | Type | Default | Purpose |
|---|---|---|---|
| `persistence.enabled` | bool | `true` | Mount a PVC at `/data`. When `false`, an `emptyDir` is used and state is lost on pod replacement. |
| `persistence.storageClass` | string | `""` | Storage class. Empty uses cluster default. |
| `persistence.accessModes` | list | `[ReadWriteOnce]` | PVC access modes. |
| `persistence.size` | string | `5Gi` | PVC capacity. |
| `persistence.existingClaim` | string | `""` | Reuse an existing PVC instead of templating one. |

#### Service and Ingress

| Key | Type | Default | Purpose |
|---|---|---|---|
| `service.type` | string | `ClusterIP` | `ClusterIP` \| `NodePort` \| `LoadBalancer`. |
| `service.port` | int | `80` | Service-side port (forwards to `config.port`). |
| `ingress.enabled` | bool | `false` | Render an Ingress resource. |
| `ingress.className` | string | `""` | `IngressClass` name. |
| `ingress.annotations` | map | `{}` | Free-form annotations (cert-manager, NGINX rewrites, …). |
| `ingress.hosts` | list | `[branchwork.local /]` | List of `{host, paths: [{path, pathType}]}` entries. |
| `ingress.tls` | list | `[]` | List of `{secretName, hosts}` entries. |

#### Resources, scheduling, autoscaling

| Key | Type | Default | Purpose |
|---|---|---|---|
| `resources.requests.cpu` | string | `100m` | Pod CPU request. |
| `resources.requests.memory` | string | `128Mi` | Pod memory request. |
| `resources.limits.cpu` | string | `"1"` | Pod CPU limit. |
| `resources.limits.memory` | string | `512Mi` | Pod memory limit. |
| `autoscaling.enabled` | bool | `false` | Render an HPA. Useful only with `database.mode: postgres` (SQLite mode caps at one replica). |
| `autoscaling.minReplicas` | int | `1` | HPA min. |
| `autoscaling.maxReplicas` | int | `5` | HPA max. |
| `autoscaling.targetCPUUtilizationPercentage` | int | `80` | HPA target. |
| `nodeSelector` | map | `{}` | Standard k8s node selector. |
| `tolerations` | list | `[]` | Standard k8s tolerations. |
| `affinity` | map | `{}` | Standard k8s affinity. |

#### Security and pod metadata

| Key | Type | Default | Purpose |
|---|---|---|---|
| `serviceAccount.create` | bool | `true` | Render a `ServiceAccount`. |
| `serviceAccount.annotations` | map | `{}` | SA annotations (e.g. for IRSA, Workload Identity). |
| `serviceAccount.name` | string | `""` | Reuse an existing SA name. |
| `podAnnotations` | map | `{}` | Pod-level annotations. |
| `podSecurityContext` | map | `{fsGroup: 1000}` | Pod `securityContext`. |
| `securityContext` | map | `{runAsNonRoot: true, runAsUser: 1000, readOnlyRootFilesystem: true, allowPrivilegeEscalation: false}` | Container `securityContext`. The `tmp` `emptyDir` mount lets the read-only root coexist with `/tmp` writes. |
| `extraEnv` | list | `[]` | Free-form list of `{name, value}` or `{name, valueFrom}`. Anything not covered by the chart goes here. |

#### SMTP (optional, for budget-alert emails)

The SMTP block is rendered only when `smtp.host` is set or
`smtp.existingSecret` is provided.

| Key | Type | Default | Purpose |
|---|---|---|---|
| `smtp.host` | string | `""` | SMTP server hostname. Setting this enables the whole block. |
| `smtp.port` | int | `587` | SMTP port. |
| `smtp.from` | string | `""` | `From:` address. |
| `smtp.username` | string | `""` | SMTP username (when `existingSecret` is empty). |
| `smtp.password` | string | `""` | SMTP password (when `existingSecret` is empty). Avoid in plain values files; prefer `existingSecret`. |
| `smtp.existingSecret` | string | `""` | Name of a `Secret` with keys `SMTP_USERNAME` and `SMTP_PASSWORD`. |

The variables actually consumed by the binary are the
`BRANCHWORK_WEBHOOK_URL` and `SMTP_*` family documented in
[reference/configuration.md](../reference/configuration.md); the chart
just wires them into the Pod env.

### Templates

What the chart renders, in `deploy/helm/branchwork/templates/`:

- `deployment.yaml` — the Deployment, with liveness / readiness on
  `/health`, the `/data` mount (SQLite mode only), and the env block
  described above.
- `configmap.yaml` — non-secret config (`PORT`, `EFFORT`).
- `service.yaml` — the Service, type from `service.type`.
- `ingress.yaml` — gated on `ingress.enabled`.
- `pvc.yaml` — gated on `persistence.enabled && database.mode == sqlite && !persistence.existingClaim`.
- `hpa.yaml` — gated on `autoscaling.enabled`.
- `serviceaccount.yaml` — gated on `serviceAccount.create`.
- `_helpers.tpl` — name / labels / image-tag template helpers.
- `NOTES.txt` — post-install hints (port-forward command, mode warning).

## Terraform module

`deploy/terraform/` is a flat module — no submodules, no remote
backends — that creates an ECS Fargate service in front of an ALB,
backed by EFS for `/data`. Drop it under `terraform/` in your infra
repo, set the three required variables, run `terraform apply`.

### Apply

```sh
cd deploy/terraform
terraform init
terraform plan -var-file=example.tfvars
terraform apply -var-file=example.tfvars
```

The example file is committed:
[`deploy/terraform/example.tfvars`](../../deploy/terraform/example.tfvars).
Copy it to your own `prod.tfvars`, fill in the VPC and subnet IDs, and
adjust tags — that's the whole onboarding.

`required_version = ">= 1.5"`, `provider aws >= 5.0`. AWS region is
inferred from the provider configuration (the module reads
`data.aws_region.current` for the CloudWatch log group ARN).

### Variables

Every variable in
[`variables.tf`](../../deploy/terraform/variables.tf).

#### Required

| Variable | Type | Purpose |
|---|---|---|
| `vpc_id` | `string` | VPC that hosts the ALB, ECS tasks, and EFS. |
| `subnet_ids` | `list(string)` | Private subnets for the Fargate tasks and the EFS mount targets. |
| `public_subnet_ids` | `list(string)` | Public subnets for the ALB. |

#### Optional

| Variable | Type | Default | Purpose |
|---|---|---|---|
| `name` | `string` | `"branchwork"` | Prefix for every resource name (cluster, ALB, target group, log group, IAM roles, security groups, EFS access point). |
| `image` | `string` | `"ghcr.io/branchwork/branchwork:0.3.0"` | Container image. Override to pin a different tag — the public tag matrix is documented in [architecture/deploy.md](../architecture/deploy.md). |
| `port` | `number` | `3100` | HTTP port the container listens on; the ALB target group health-checks `/health` here. |
| `cpu` | `number` | `256` | Fargate task CPU units (256 = 0.25 vCPU). |
| `memory` | `number` | `512` | Fargate task memory in MiB. |
| `effort` | `string` | `"high"` | Agent effort level; passed as `--effort`. Same domain as the Helm `config.effort` value. |
| `webhook_url` | `string` (sensitive) | `""` | Slack / generic webhook URL; exported as `BRANCHWORK_WEBHOOK_URL` only when non-empty. |
| `certificate_arn` | `string` | `""` | ACM certificate for HTTPS. Empty → ALB serves HTTP on `:80` only. Non-empty → ALB redirects HTTP to HTTPS and serves the target group on `:443`. |
| `tags` | `map(string)` | `{}` | Tags applied to every taggable resource (cluster, log group, IAM roles, security groups, ALB, target group, listeners, EFS, task definition, service). |

### Outputs

Every output in [`outputs.tf`](../../deploy/terraform/outputs.tf).

| Output | Type | Source | Use |
|---|---|---|---|
| `alb_dns_name` | string | `aws_lb.this.dns_name` | The ALB's auto-generated DNS name. Point a Route 53 alias here. |
| `alb_url` | string | `http://${aws_lb.this.dns_name}` | HTTP URL for the dashboard. Only correct when `certificate_arn` is empty — otherwise the ALB redirects to HTTPS and you should use your own `https://` hostname. |
| `ecs_cluster_name` | string | `aws_ecs_cluster.this.name` | Cluster name; useful for `aws ecs execute-command` and CLI scripts. |
| `ecs_service_name` | string | `aws_ecs_service.this.name` | Service name; useful for `aws ecs update-service --force-new-deployment` after a `--var image=...` upgrade. |
| `efs_file_system_id` | string | `aws_efs_file_system.this.id` | EFS file system ID. The access point under `/branchwork` (uid/gid 1000) is mounted at `/data` in the task. Back this up with AWS Backup or `efs-utils`. |
| `log_group` | string | `aws_cloudwatch_log_group.this.name` | CloudWatch log group: `/ecs/${var.name}`. Tail with `aws logs tail $(terraform output -raw log_group) --follow`. |

### What it provisions

[`main.tf`](../../deploy/terraform/main.tf) creates, per `var.name`:

- **ECS cluster** with Container Insights enabled.
- **CloudWatch log group** at `/ecs/<name>`, 30-day retention.
- **IAM roles**: one execution role (with the AWS-managed
  `AmazonECSTaskExecutionRolePolicy`) and one empty task role you can
  attach further policies to.
- **Security groups**: one for the ALB (`:80`, `:443` open to the
  world), one for the ECS tasks (`:port` open from the ALB SG only),
  one for EFS (`:2049` open from the ECS SG only).
- **ALB + target group + HTTP listener**, plus an HTTPS listener and
  HTTP→HTTPS redirect when `certificate_arn` is set.
- **EFS file system** (encrypted), one mount target per
  `subnet_ids` entry, and an access point at `/branchwork` with
  POSIX uid/gid `1000` and `0755` permissions.
- **ECS task definition** (Fargate, `awsvpc`) that mounts the EFS
  access point at `/data` with transit encryption, runs
  `branchwork-server --port <port> --effort <effort> --claude-dir /data`,
  and ships logs to CloudWatch.
- **ECS service** with `desired_count = 1` and
  `launch_type = FARGATE`, registered with the ALB target group.

The module does not create the VPC or subnets — bring your own. It
also does not provision Postgres; the same caveat as the Helm chart
applies, so the EFS-backed `/data` directory holds the SQLite database
and the plan files.

## Upgrades

For both surfaces, the canonical upgrade path is **bump the image tag
and reapply** — the Branchwork binary's `db::init` runs
idempotent migrations on every boot
([architecture/persistence.md § Schema migrations](../architecture/persistence.md#schema-migrations)),
and the per-agent session daemons survive a server restart via
`cleanup_and_reattach`
([architecture/session-daemon.md](../architecture/session-daemon.md)).

- **Helm**: `helm upgrade branchwork ./deploy/helm/branchwork -f
  my-values.yaml --set image.tag=<new>` (or bump `image.tag` in the
  values file).
- **Terraform**: `terraform apply -var image=ghcr.io/branchwork/branchwork:<new>`.
  The ECS service will roll the task definition automatically.

For SQLite → Postgres migrations, rollback procedures, and
backup/restore details, see
[operations/upgrades-and-migrations.md](upgrades-and-migrations.md).

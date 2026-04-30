variable "name" {
  description = "Resource name prefix"
  type        = string
  default     = "branchwork"
}

variable "image" {
  description = "Container image (e.g. ghcr.io/branchwork/branchwork:0.3.0)"
  type        = string
  default     = "ghcr.io/branchwork/branchwork:0.3.0"
}

variable "port" {
  description = "HTTP port the container listens on"
  type        = number
  default     = 3100
}

variable "cpu" {
  description = "Fargate task CPU units (256 = 0.25 vCPU)"
  type        = number
  default     = 256
}

variable "memory" {
  description = "Fargate task memory in MiB"
  type        = number
  default     = 512
}

variable "vpc_id" {
  description = "VPC ID for the ECS service"
  type        = string
}

variable "subnet_ids" {
  description = "Subnet IDs for the ECS tasks"
  type        = list(string)
}

variable "public_subnet_ids" {
  description = "Public subnet IDs for the ALB"
  type        = list(string)
}

variable "effort" {
  description = "Agent effort level: low | medium | high | max"
  type        = string
  default     = "high"
}

variable "webhook_url" {
  description = "Optional Slack / webhook URL for agent notifications"
  type        = string
  default     = ""
  sensitive   = true
}

variable "certificate_arn" {
  description = "ACM certificate ARN for HTTPS (optional — ALB uses HTTP if empty)"
  type        = string
  default     = ""
}

variable "tags" {
  description = "Tags to apply to all resources"
  type        = map(string)
  default     = {}
}

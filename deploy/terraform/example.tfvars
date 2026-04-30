# Example: deploy Branchwork on AWS ECS Fargate
#
# Usage:
#   terraform init
#   terraform plan -var-file=example.tfvars
#   terraform apply -var-file=example.tfvars

name  = "branchwork"
image = "ghcr.io/branchwork/branchwork:0.3.0"

vpc_id            = "vpc-0123456789abcdef0"
subnet_ids        = ["subnet-aaa", "subnet-bbb"]
public_subnet_ids = ["subnet-pub-aaa", "subnet-pub-bbb"]

# Optional: HTTPS with ACM certificate
# certificate_arn = "arn:aws:acm:us-east-1:123456789:certificate/abc-123"

effort = "high"

tags = {
  Environment = "production"
  Project     = "branchwork"
}

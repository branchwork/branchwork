output "alb_dns_name" {
  description = "DNS name of the Application Load Balancer"
  value       = aws_lb.this.dns_name
}

output "alb_url" {
  description = "HTTP URL for the dashboard"
  value       = "http://${aws_lb.this.dns_name}"
}

output "ecs_cluster_name" {
  description = "ECS cluster name"
  value       = aws_ecs_cluster.this.name
}

output "ecs_service_name" {
  description = "ECS service name"
  value       = aws_ecs_service.this.name
}

output "efs_file_system_id" {
  description = "EFS file system ID used for persistent storage"
  value       = aws_efs_file_system.this.id
}

output "log_group" {
  description = "CloudWatch log group"
  value       = aws_cloudwatch_log_group.this.name
}

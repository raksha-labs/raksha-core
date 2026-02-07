output "ecs_cluster_name" {
  value       = aws_ecs_cluster.defi_surv.name
  description = "ECS cluster used for DeFi surveillance services"
}

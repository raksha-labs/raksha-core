output "vpc_id" {
  value       = aws_vpc.main.id
  description = "VPC ID"
}

output "vpc_cidr" {
  value       = aws_vpc.main.cidr_block
  description = "VPC CIDR"
}

output "public_subnet_ids" {
  value       = aws_subnet.public[*].id
  description = "Public subnet IDs"
}

output "private_subnet_ids" {
  value       = aws_subnet.private[*].id
  description = "Private subnet IDs"
}

output "selected_azs" {
  value       = local.selected_azs
  description = "Selected availability zones"
}

output "internet_gateway_id" {
  value       = aws_internet_gateway.main.id
  description = "Internet gateway ID"
}

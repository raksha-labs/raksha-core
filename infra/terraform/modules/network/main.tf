data "aws_availability_zones" "available" {
  state = "available"
}

locals {
  selected_azs      = var.single_az ? [data.aws_availability_zones.available.names[0]] : slice(data.aws_availability_zones.available.names, 0, var.az_count)
  subnet_count      = length(local.selected_azs)
  nat_gateway_count = var.create_nat_gateway ? (var.nat_gateway_per_az ? local.subnet_count : 1) : 0
}

resource "aws_vpc" "main" {
  cidr_block           = var.vpc_cidr
  enable_dns_hostnames = true
  enable_dns_support   = true

  tags = merge(var.tags, {
    Name = "raksha-${var.environment}-vpc"
  })
}

resource "aws_internet_gateway" "main" {
  vpc_id = aws_vpc.main.id

  tags = merge(var.tags, {
    Name = "raksha-${var.environment}-igw"
  })
}

resource "aws_subnet" "public" {
  count                   = local.subnet_count
  vpc_id                  = aws_vpc.main.id
  cidr_block              = cidrsubnet(var.vpc_cidr, 8, count.index)
  availability_zone       = local.selected_azs[count.index]
  map_public_ip_on_launch = true

  tags = merge(var.tags, {
    Name = "raksha-${var.environment}-public-${count.index + 1}"
    Tier = "public"
  })
}

resource "aws_subnet" "private" {
  count             = local.subnet_count
  vpc_id            = aws_vpc.main.id
  cidr_block        = cidrsubnet(var.vpc_cidr, 8, count.index + 100)
  availability_zone = local.selected_azs[count.index]

  tags = merge(var.tags, {
    Name = "raksha-${var.environment}-private-${count.index + 1}"
    Tier = "private"
  })
}

resource "aws_eip" "nat" {
  count  = local.nat_gateway_count
  domain = "vpc"

  tags = merge(var.tags, {
    Name = "raksha-${var.environment}-nat-eip-${count.index + 1}"
  })
}

resource "aws_nat_gateway" "main" {
  count         = local.nat_gateway_count
  allocation_id = aws_eip.nat[count.index].id
  subnet_id     = aws_subnet.public[var.nat_gateway_per_az ? count.index : 0].id

  tags = merge(var.tags, {
    Name = "raksha-${var.environment}-nat-${count.index + 1}"
  })

  depends_on = [aws_internet_gateway.main]
}

resource "aws_route_table" "public" {
  vpc_id = aws_vpc.main.id

  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.main.id
  }

  tags = merge(var.tags, {
    Name = "raksha-${var.environment}-public-rt"
  })
}

resource "aws_route_table" "private" {
  count  = local.subnet_count
  vpc_id = aws_vpc.main.id

  tags = merge(var.tags, {
    Name = "raksha-${var.environment}-private-rt-${count.index + 1}"
  })
}

resource "aws_route" "private_default" {
  count                  = var.create_nat_gateway ? local.subnet_count : 0
  route_table_id         = aws_route_table.private[count.index].id
  destination_cidr_block = "0.0.0.0/0"
  nat_gateway_id         = aws_nat_gateway.main[var.nat_gateway_per_az ? count.index : 0].id
}

resource "aws_route_table_association" "public" {
  count          = local.subnet_count
  subnet_id      = aws_subnet.public[count.index].id
  route_table_id = aws_route_table.public.id
}

resource "aws_route_table_association" "private" {
  count          = local.subnet_count
  subnet_id      = aws_subnet.private[count.index].id
  route_table_id = aws_route_table.private[count.index].id
}

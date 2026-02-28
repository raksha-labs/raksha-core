locals {
  branch_subjects = [for branch in var.github_allowed_branches : "repo:${var.github_org}/${var.github_repo}:ref:refs/heads/${branch}"]
  tag_subjects    = var.allow_tag_refs ? ["repo:${var.github_org}/${var.github_repo}:ref:refs/tags/*"] : []
  pr_subjects     = var.allow_pull_request ? ["repo:${var.github_org}/${var.github_repo}:pull_request"] : []
  subjects        = concat(local.branch_subjects, local.tag_subjects, local.pr_subjects)

  effective_oidc_provider_arn = var.create_oidc_provider ? aws_iam_openid_connect_provider.github[0].arn : var.oidc_provider_arn

  trust_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect = "Allow"
        Principal = {
          Federated = local.effective_oidc_provider_arn
        }
        Action = "sts:AssumeRoleWithWebIdentity"
        Condition = {
          StringEquals = {
            "token.actions.githubusercontent.com:aud" = "sts.amazonaws.com"
          }
          StringLike = {
            "token.actions.githubusercontent.com:sub" = local.subjects
          }
        }
      }
    ]
  })
}

resource "aws_iam_openid_connect_provider" "github" {
  count = var.create_oidc_provider ? 1 : 0

  url = "https://token.actions.githubusercontent.com"

  client_id_list = ["sts.amazonaws.com"]

  thumbprint_list = [
    "6938fd4d98bab03faadb97b34396831e3780aea1"
  ]

  tags = merge(var.tags, {
    Name = "github-actions-oidc"
  })
}

resource "aws_iam_role" "images" {
  name               = "raksha-${var.environment}-github-images-role"
  assume_role_policy = local.trust_policy

  tags = var.tags
}

resource "aws_iam_role_policy" "images" {
  name = "raksha-${var.environment}-github-images-policy"
  role = aws_iam_role.images.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect = "Allow"
        Action = [
          "ecr:GetAuthorizationToken",
          "ecr:BatchCheckLayerAvailability",
          "ecr:CompleteLayerUpload",
          "ecr:UploadLayerPart",
          "ecr:InitiateLayerUpload",
          "ecr:PutImage",
          "ecr:BatchGetImage",
          "ecr:GetDownloadUrlForLayer",
          "ecr:DescribeRepositories",
          "ecr:CreateRepository"
        ]
        Resource = "*"
      }
    ]
  })
}

resource "aws_iam_role" "infra" {
  name               = "raksha-${var.environment}-github-infra-role"
  assume_role_policy = local.trust_policy

  tags = var.tags
}

resource "aws_iam_role_policy" "infra" {
  name = "raksha-${var.environment}-github-infra-policy"
  role = aws_iam_role.infra.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect = "Allow"
        Action = [
          "ec2:*",
          "elasticloadbalancing:*",
          "ecs:*",
          "ecr:*",
          "iam:*",
          "logs:*",
          "cloudwatch:*",
          "application-autoscaling:*",
          "autoscaling:*",
          "servicediscovery:*",
          "secretsmanager:*",
          "kms:*",
          "rds:*",
          "elasticache:*",
          "sns:*",
          "budgets:*",
          "ce:*",
          "s3:*",
          "dynamodb:*",
          "wafv2:*",
          "acm:*",
          "codedeploy:*",
          "events:*",
          "ssm:*"
        ]
        Resource = "*"
      }
    ]
  })
}

resource "aws_iam_role" "deploy" {
  name               = "raksha-${var.environment}-github-deploy-role"
  assume_role_policy = local.trust_policy

  tags = var.tags
}

resource "aws_iam_role_policy" "deploy" {
  name = "raksha-${var.environment}-github-deploy-policy"
  role = aws_iam_role.deploy.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect = "Allow"
        Action = [
          "ecs:DescribeServices",
          "ecs:DescribeTaskDefinition",
          "ecs:RegisterTaskDefinition",
          "ecs:UpdateService",
          "ecs:DescribeClusters",
          "ecs:ListServices",
          "ecs:TagResource",
          "codedeploy:CreateDeployment",
          "codedeploy:GetDeployment",
          "codedeploy:GetDeploymentGroup",
          "codedeploy:RegisterApplicationRevision",
          "application-autoscaling:RegisterScalableTarget",
          "application-autoscaling:PutScalingPolicy",
          "cloudwatch:DescribeAlarms",
          "logs:DescribeLogGroups",
          "iam:PassRole",
          "elasticloadbalancing:DescribeTargetGroups"
        ]
        Resource = "*"
      }
    ]
  })
}

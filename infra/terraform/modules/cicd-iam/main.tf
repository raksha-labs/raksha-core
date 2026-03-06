locals {
  trusted_repositories = distinct(concat([var.github_repo], var.github_additional_repositories))
  branch_subjects = flatten([
    for repo in local.trusted_repositories : [
      for branch in var.github_allowed_branches : "repo:${var.github_org}/${repo}:ref:refs/heads/${branch}"
    ]
  ])
  tag_subjects = var.allow_tag_refs ? [
    for repo in local.trusted_repositories : "repo:${var.github_org}/${repo}:ref:refs/tags/*"
  ] : []
  pr_subjects = var.allow_pull_request ? [
    for repo in local.trusted_repositories : "repo:${var.github_org}/${repo}:pull_request"
  ] : []
  env_subjects = flatten([
    for repo in local.trusted_repositories : [
      for environment in var.github_allowed_environments : "repo:${var.github_org}/${repo}:environment:${environment}"
    ]
  ])
  subjects = concat(local.branch_subjects, local.tag_subjects, local.pr_subjects, local.env_subjects)

  # Fallback to the account's standard GitHub OIDC provider ARN when not explicitly provided.
  effective_oidc_provider_arn = var.create_oidc_provider ? aws_iam_openid_connect_provider.github[0].arn : coalesce(var.oidc_provider_arn, "arn:aws:iam::${data.aws_caller_identity.current.account_id}:oidc-provider/token.actions.githubusercontent.com")

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

data "aws_caller_identity" "current" {}

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
          "ecr:GetAuthorizationToken",
          "ecr:DescribeRepositories",
          "ecr:PutImageTagMutability",
          "ecr:PutLifecyclePolicy",
          "ecr:TagResource",
          "servicediscovery:ListTagsForResource",
          "servicediscovery:TagResource",
          "servicediscovery:UntagResource",
          "codedeploy:CreateDeployment",
          "codedeploy:GetDeployment",
          "codedeploy:GetDeploymentGroup",
          "codedeploy:RegisterApplicationRevision",
          "application-autoscaling:RegisterScalableTarget",
          "application-autoscaling:PutScalingPolicy",
          "cloudwatch:DescribeAlarms",
          "logs:DescribeLogGroups",
          "iam:PassRole",
          "elasticloadbalancing:DescribeTargetGroups",
          "elasticloadbalancing:DescribeListenerAttributes",
          "elasticloadbalancing:ModifyListenerAttributes",
          "secretsmanager:RestoreSecret"
        ]
        Resource = "*"
      }
    ]
  })
}

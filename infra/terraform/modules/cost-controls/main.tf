locals {
  notifications = [
    {
      type      = "ACTUAL"
      threshold = 60
    },
    {
      type      = "ACTUAL"
      threshold = 80
    },
    {
      type      = "FORECASTED"
      threshold = 100
    }
  ]
}

resource "aws_sns_topic" "budget_alerts" {
  name = "${var.name_prefix}-${var.environment}-budget-alerts"

  tags = merge(var.tags, {
    Name = "${var.name_prefix}-${var.environment}-budget-alerts"
  })
}

resource "aws_sns_topic_subscription" "budget_email" {
  for_each = toset(var.alert_email_addresses)

  topic_arn = aws_sns_topic.budget_alerts.arn
  protocol  = "email"
  endpoint  = each.key
}

resource "aws_budgets_budget" "monthly" {
  name         = "${var.name_prefix}-${var.environment}-monthly-budget"
  budget_type  = "COST"
  limit_unit   = "USD"
  limit_amount = tostring(var.budget_limit_usd)
  time_unit    = "MONTHLY"

  dynamic "notification" {
    for_each = local.notifications

    content {
      comparison_operator        = "GREATER_THAN"
      threshold                  = notification.value.threshold
      threshold_type             = "PERCENTAGE"
      notification_type          = notification.value.type
      subscriber_sns_topic_arns  = [aws_sns_topic.budget_alerts.arn]
      subscriber_email_addresses = var.alert_email_addresses
    }
  }
}

resource "aws_ce_anomaly_monitor" "main" {
  name              = "${var.name_prefix}-${var.environment}-anomaly-monitor"
  monitor_type      = "DIMENSIONAL"
  monitor_dimension = "SERVICE"
}

resource "aws_ce_anomaly_subscription" "main" {
  name      = "${var.name_prefix}-${var.environment}-anomaly-subscription"
  frequency = "DAILY"

  monitor_arn_list = [aws_ce_anomaly_monitor.main.arn]

  subscriber {
    type    = "SNS"
    address = aws_sns_topic.budget_alerts.arn
  }

  dynamic "subscriber" {
    for_each = toset(var.alert_email_addresses)

    content {
      type    = "EMAIL"
      address = subscriber.key
    }
  }

  threshold_expression {
    and {
      dimension {
        key           = "ANOMALY_TOTAL_IMPACT_ABSOLUTE"
        values        = ["20"]
        match_options = ["GREATER_THAN_OR_EQUAL"]
      }
    }
  }
}

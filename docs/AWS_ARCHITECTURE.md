# AWS Architecture

This document describes how the `raksha-core` AWS infrastructure fits together in Terraform, with focus on VPC layout, NAT egress, Route 53 private service discovery, ALBs, ECS, and the managed data tier.

The diagrams below represent the standard managed-data topology used by `stage` and `prod`, and now also by `test` when `enable_managed_data = true`.

## Rendered AWS Diagram

![Raksha Core AWS Architecture](assets/raksha-core-aws-architecture.png)

Source: [raksha_core_aws_architecture.py](/Users/sanjaya.rajamantrilage/workspace/blockchain/raksha-labs/raksha-core/docs/diagrams/raksha_core_aws_architecture.py)

## Network Topology

```mermaid
flowchart TB
  Internet[(Internet / Operators / External APIs)]
  AWSAPIs[(AWS APIs\nECR / Secrets Manager / CloudWatch / SSM)]
  RPC[(Chain RPC / WebSocket\nCEX / DEX / Oracle feeds)]

  subgraph Region["AWS Region (eu-west-1)"]
    IGW[Internet Gateway]

    subgraph VPC["Raksha VPC"]
      direction TB

      subgraph Public["Public Subnets"]
        PublicALB[Public ALB]
        NatGW[NAT Gateway]
      end

      subgraph Private["Private Subnets"]
        AdminALB[Internal Admin ALB]
        ECS[ECS Cluster\nFargate and/or EC2-backed tasks]
        RDS[(PostgreSQL / RDS)]
        Redis[(Redis / ElastiCache)]
        SD[Cloud Map + Route 53\nPrivate DNS namespace\nraksha-<env>.local]
      end
    end
  end

  Internet --> IGW
  IGW --> PublicALB
  IGW --> NatGW

  PublicALB --> ECS
  AdminALB --> ECS

  ECS --> RDS
  ECS --> Redis

  ECS --> SD
  SD --> ECS

  ECS --> NatGW
  NatGW --> IGW
  IGW --> AWSAPIs
  IGW --> RPC
```

## How It Works

- The VPC spans at least 2 AZs when managed data is enabled, so RDS Multi-AZ and ElastiCache replication groups can use separate private subnets.
- Public subnets hold the internet-facing entry points: Internet Gateway attachment, public ALB, and NAT gateway.
- Private subnets hold the application runtime and stateful services: ECS tasks, internal admin ALB, RDS, Redis, and the private DNS namespace.
- ECS tasks in private subnets do not need inbound internet access. They use the NAT gateway for outbound calls to AWS services and external market or blockchain providers.
- The public ALB sends user or API traffic to public-facing ECS services.
- The internal admin ALB stays inside the VPC and routes admin traffic only from allowed private CIDRs.
- RDS and Redis are reachable only from the ECS task security group.

## Route 53 Private Service Discovery

`raksha-core` uses AWS Cloud Map with a private DNS namespace, which creates Route 53 private DNS records scoped to the VPC.

```mermaid
flowchart LR
  subgraph VPC["Raksha VPC"]
    TaskA[indexer task]
    TaskB[detector task]
    TaskC[orchestrator task]
    TaskD[finality task]

    NS[Cloud Map service registry\nRoute 53 private namespace\nraksha-<env>.local]

    Svc1[indexer.raksha-<env>.local]
    Svc2[detector.raksha-<env>.local]
    Svc3[orchestrator.raksha-<env>.local]
    Svc4[finality.raksha-<env>.local]
  end

  TaskA <-- DNS resolve --> NS
  TaskB <-- DNS resolve --> NS
  TaskC <-- DNS resolve --> NS
  TaskD <-- DNS resolve --> NS

  NS --> Svc1
  NS --> Svc2
  NS --> Svc3
  NS --> Svc4
```

## Route 53, VPC, and NAT Relationship

- Route 53 private DNS is for east-west traffic inside the VPC.
- NAT is for north-south outbound traffic from private subnets to the internet or AWS public endpoints.
- These solve different problems:
  - Route 53 private namespace: lets services find each other by stable names inside the VPC.
  - NAT gateway: lets private workloads reach external dependencies without becoming publicly reachable.
- Typical request paths:
  - Internal service call: `detector` resolves `orchestrator.raksha-<env>.local` through Route 53 private DNS and connects directly over the VPC.
  - Outbound dependency call: `indexer` reaches Alchemy, exchange APIs, or AWS Secrets Manager via the NAT gateway.
  - Inbound user traffic: client reaches public ALB through the internet gateway, then ALB routes to ECS tasks.

## Raksha-Core AWS Components

- `modules/network`
  - VPC, public/private subnets, route tables, NAT, IGW
- `modules/security`
  - security groups for public ALB, internal admin ALB, ECS tasks, ECS instances, RDS, Redis
- `modules/compute-ecs`
  - ECS cluster, task definitions, services, public/internal ALBs, Cloud Map private namespace
- `modules/data-prod`
  - KMS-encrypted RDS, ElastiCache, and Secrets Manager secrets
- `modules/observability`
  - CloudWatch log groups, alarms
- `modules/cost-controls`
  - budget and anomaly alerting

## Environment Notes

- `test`
  - With `enable_managed_data = true`, the environment uses at least 2 AZs.
  - With `create_nat_gateway = false`, Fargate tasks can be placed in public subnets and get public IPs for low-cost test mode.
- `stage`
  - Managed data, 2 AZs, NAT enabled, internal admin ALB available.
- `prod`
  - Managed data, stronger HA posture, optional per-AZ NAT, public HTTPS/WAF support.

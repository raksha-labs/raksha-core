from diagrams import Cluster, Diagram, Edge
from diagrams.aws.compute import ECS
from diagrams.aws.compute import ECR
from diagrams.aws.database import ElastiCache
from diagrams.aws.database import RDSPostgresqlInstance
from diagrams.aws.management import Cloudwatch, SystemsManager
from diagrams.aws.network import ELB, InternetGateway, NATGateway, PrivateSubnet, PublicSubnet
from diagrams.aws.network import Route53HostedZone, VPC
from diagrams.aws.security import KMS, SecretsManager
from diagrams.onprem.client import Users
from diagrams.onprem.network import Internet


graph_attr = {
    "pad": "0.4",
    "nodesep": "0.6",
    "ranksep": "1.0",
    "splines": "ortho",
    "fontname": "Helvetica",
    "fontsize": "16",
    "labelloc": "t",
}


with Diagram(
    "Raksha Core AWS Architecture",
    filename="raksha-core/docs/assets/raksha-core-aws-architecture",
    outformat="png",
    show=False,
    direction="TB",
    graph_attr=graph_attr,
):
    users = Users("Clients / Operators")
    external = Internet("External providers\nRPC / CEX / DEX / Oracle")

    with Cluster("AWS Region: eu-west-1"):
        igw = InternetGateway("Internet Gateway")

        with Cluster("Raksha VPC"):
            vpc = VPC("10.x.0.0/16")

            with Cluster("Public subnets"):
                public_subnets = PublicSubnet("AZ A / AZ B")
                public_alb = ELB("Public ALB")
                nat = NATGateway("NAT Gateway")

            with Cluster("Private subnets"):
                private_subnets = PrivateSubnet("AZ A / AZ B")
                admin_alb = ELB("Internal Admin ALB")
                cloudmap = Route53HostedZone("Cloud Map + Route 53\nraksha-<env>.local")

                with Cluster("ECS services"):
                    indexer = ECS("indexer")
                    detector = ECS("detector")
                    orchestrator = ECS("orchestrator")
                    finality = ECS("finality")
                    history = ECS("history-worker")

                database = RDSPostgresqlInstance("RDS PostgreSQL")
                cache = ElastiCache("Redis")

            with Cluster("Shared AWS services"):
                ecr = ECR("ECR")
                secrets = SecretsManager("Secrets Manager")
                kms = KMS("KMS")
                cloudwatch = Cloudwatch("CloudWatch")
                ssm = SystemsManager("SSM Parameter Store")

    users >> igw >> public_alb
    igw >> nat

    vpc >> public_subnets
    vpc >> private_subnets

    public_alb >> Edge(label="public HTTP/HTTPS") >> indexer
    public_alb >> detector
    public_alb >> orchestrator
    admin_alb >> Edge(label="private admin access") >> orchestrator

    cloudmap >> Edge(label="private DNS") >> indexer
    cloudmap >> detector
    cloudmap >> orchestrator
    cloudmap >> finality
    cloudmap >> history

    indexer >> database
    detector >> database
    orchestrator >> database
    finality >> database
    history >> database

    indexer >> cache
    detector >> cache
    orchestrator >> cache
    finality >> cache
    history >> cache

    indexer >> Edge(label="outbound via NAT") >> nat
    detector >> nat
    orchestrator >> nat
    finality >> nat
    history >> nat

    nat >> external
    nat >> ecr
    nat >> secrets
    nat >> kms
    nat >> cloudwatch
    nat >> ssm

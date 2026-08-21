locals {
  # 터널 로컬 포트: 로컬 docker compose(13306~13308)와 겹치지 않는 대역.
  # env 인덱스마다 10씩 벌린다: prd 1431x, dev 1432x. x: 0 writer, 1 reader,
  # 2 mysql, 3 mysql-ro, 4 aurora84(있는 env 만).
  tunnel_targets = merge(concat(
    [
      for idx, env in var.seed_environments : {
        "${env}-aurora-writer" = {
          host       = aws_rds_cluster.aurora[env].endpoint
          local_port = 14310 + idx * 10
        }
        "${env}-aurora-reader" = {
          host       = aws_rds_cluster.aurora[env].reader_endpoint
          local_port = 14311 + idx * 10
        }
        "${env}-mysql" = {
          host       = aws_db_instance.mysql[env].address
          local_port = 14312 + idx * 10
        }
        "${env}-mysql-ro" = {
          host       = aws_db_instance.mysql_replica[env].address
          local_port = 14313 + idx * 10
        }
      }
    ],
    [
      for env in var.aurora84_environments : {
        "${env}-aurora84" = {
          host       = aws_rds_cluster.aurora84[env].endpoint
          local_port = 14314 + index(var.seed_environments, env) * 10
        }
      }
    ],
  )...)

  all_db_identifiers = concat(
    [for _, v in aws_rds_cluster_instance.aurora : v.identifier],
    [for _, v in aws_rds_cluster_instance.aurora84 : v.identifier],
    [for _, v in aws_db_instance.mysql : v.identifier],
    [for _, v in aws_db_instance.mysql_replica : v.identifier],
  )
}

output "aurora" {
  description = "환경별 Aurora 클러스터: 엔드포인트·IAM DB Auth 용 리소스 ID·마스터 시크릿."
  value = {
    for env, c in aws_rds_cluster.aurora : env => {
      writer_endpoint     = c.endpoint
      reader_endpoint     = c.reader_endpoint
      cluster_resource_id = c.cluster_resource_id
      master_secret_arn   = try(c.master_user_secret[0].secret_arn, null)
    }
  }
}

output "aurora84" {
  description = "Aurora MySQL 8.4 클러스터 (기본 prd 1곳, writer 1대)."
  value = {
    for env, c in aws_rds_cluster.aurora84 : env => {
      writer_endpoint     = c.endpoint
      cluster_resource_id = c.cluster_resource_id
      master_secret_arn   = try(c.master_user_secret[0].secret_arn, null)
    }
  }
}

output "mysql" {
  description = "환경별 RDS MySQL: primary/replica 주소·리소스 ID·마스터 시크릿(자체 관리)."
  value = {
    for env, i in aws_db_instance.mysql : env => {
      primary_address     = i.address
      replica_address     = aws_db_instance.mysql_replica[env].address
      primary_resource_id = i.resource_id
      replica_resource_id = aws_db_instance.mysql_replica[env].resource_id
      # RDS 관리형이 아니라 우리가 만든 시크릿이다 (리플리카 호환, main.tf 참조).
      master_secret_arn = aws_secretsmanager_secret.mysql_master.arn
    }
  }
}

output "db_auth_resource_ids" {
  description = "40-compute 의 db_auth_resource_ids 에 넣을 값 (rds-db:connect 스코핑)."
  value = concat(
    [for _, c in aws_rds_cluster.aurora : c.cluster_resource_id],
    [for _, c in aws_rds_cluster.aurora84 : c.cluster_resource_id],
    [for _, i in aws_db_instance.mysql : i.resource_id],
    [for _, i in aws_db_instance.mysql_replica : i.resource_id],
  )
}

output "dbmon_instance_ids" {
  description = "우리 키 포맷(<account>/<region>/<identifier>)의 instance_id (M0-2a)."
  value = {
    for id in local.all_db_identifiers :
    id => "${data.aws_caller_identity.current.account_id}/${var.region}/${id}"
  }
}

output "bastion_instance_id" {
  description = "SSM 포트 포워딩 타깃. scripts/db-tunnel.sh 가 읽는다."
  value       = aws_instance.bastion.id
}

output "tunnel_targets" {
  description = "scripts/db-tunnel.sh 의 타깃 맵: 이름 → { host, local_port }."
  value       = local.tunnel_targets
}

output "slowlog_log_group_arns" {
  description = "40-compute 의 slowlog_log_group_arns 에 넣을 값."
  value = concat(
    [for k, g in aws_cloudwatch_log_group.aurora : g.arn if endswith(g.name, "/slowquery")],
    [for k, g in aws_cloudwatch_log_group.aurora84 : g.arn if endswith(g.name, "/slowquery")],
    [for k, g in aws_cloudwatch_log_group.mysql : g.arn if endswith(g.name, "/slowquery")],
  )
}

output "vpn_endpoint_id" {
  description = "Client VPN endpoint. scripts/vpn.sh 가 .ovpn 생성에 쓴다."
  value       = aws_ec2_client_vpn_endpoint.seed.id
}

output "vpn_client_cert" {
  description = ".ovpn 에 들어갈 클라이언트 인증서 (scripts/vpn.sh 가 읽는다)."
  value       = tls_locally_signed_cert.vpn_client.cert_pem
  sensitive   = true
}

output "vpn_client_key" {
  description = ".ovpn 에 들어갈 클라이언트 개인키 (scripts/vpn.sh 가 읽는다)."
  value       = tls_private_key.vpn_client.private_key_pem
  sensitive   = true
}

output "reboot_reminder" {
  description = "정적 파라미터는 재부팅해야 적용된다."
  value       = <<-EOT
    ⚠ 파라미터 그룹의 정적 항목(performance_schema, max_digest_length 등)은
      재부팅 후에 적용된다. apply 직후 확인하면 기본값이 보인다.

    %{for id in local.all_db_identifiers~}
    aws rds reboot-db-instance --db-instance-identifier ${id}
    %{endfor~}

      확인 (터널 연결 후):
      SELECT @@max_digest_length, @@performance_schema_max_digest_length,
             @@performance_schema_max_sql_text_length, @@performance_schema_digests_size;
  EOT
}

output "ecr_repository_url" {
  description = "이미지를 푸시할 곳."
  value       = aws_ecr_repository.app.repository_url
}

output "cluster_name" {
  value = aws_ecs_cluster.main.name
}

output "service_name" {
  value = aws_ecs_service.app.name
}

output "task_role_arn" {
  description = "부트스트랩 SQL 의 GRANT 대상 IAM 주체."
  value       = aws_iam_role.task.arn
}

output "alb_dns_name" {
  value = var.enable_alb ? aws_lb.app[0].dns_name : null
}

output "push_commands" {
  description = "이미지 빌드·푸시 절차. `terraform output -raw push_commands`"
  value       = <<-EOT
    # arm64 로 빌드해야 한다 (태스크가 Graviton 이다)
    TAG=$(git rev-parse --short HEAD)
    aws ecr get-login-password --region ${var.region} \
      | docker login --username AWS --password-stdin ${split("/", aws_ecr_repository.app.repository_url)[0]}
    docker build --platform linux/arm64 -t ${aws_ecr_repository.app.repository_url}:$TAG .
    docker push ${aws_ecr_repository.app.repository_url}:$TAG
    # 그 태그로 apply
    terraform apply -var image_tag=$TAG
  EOT
}

output "exec_command" {
  description = "컨테이너에 붙는 명령 (enable_ecs_exec=true 필요)."
  value       = <<-EOT
    TASK=$(aws ecs list-tasks --cluster ${aws_ecs_cluster.main.name} \
      --service-name ${aws_ecs_service.app.name} --query 'taskArns[0]' --output text)
    aws ecs execute-command --cluster ${aws_ecs_cluster.main.name} --task $TASK \
      --container dbmon --interactive --command /bin/sh
  EOT
}

# 17. 로드맵과 할일 목록

## 0. 마일스톤 개요

| M | 이름 | 목표 | 산출물 | 선행 |
|---|---|---|---|---|
| M0 | 기반 | 저장소·워크스페이스·CI·로컬 환경 | 빌드되는 빈 껍데기 + CI 초록 | — |
| M1 | 검증 스파이크 | 설계의 위험한 전제 **17개**를 실측 검증 | OPEN-QUESTIONS 해소 + 문서 갱신 | M0 |
| M2 | 제어면 | RDS 탐색·환경 분류·인스턴스 레지스트리·자가진단 | 인스턴스 목록 API | M1 |
| M3 | 부트스트랩 | 마스터 자격증명 3소스 + 모니터링 계정 생성 + IAM DB Auth 연결 | 비밀번호 없이 대상 DB 접속 | M2 |
| M4 | 수집기 | 슬로우 쿼리 캡처 + in-flight 플랜 + 다이제스트 롤업 | DynamoDB에 데이터 적재 | M3 |
| M5 | 인증·웹 셸 | Cognito + RBAC + SPA 골격 + 핵심 3화면 | 로그인해서 슬로우 쿼리·다이제스트 조회 | M4 |
| M6 | 메트릭 | 자체 실시간 지표 + CloudWatch + WebSocket | 플릿 개요 + 인스턴스 상세 | M5 |
| M7 | 아카이브 | 증분 내보내기 → Iceberg + 조회 라우팅 | 31일 초과 조회 | M4 |
| M8 | 확장 관측 | 복제·락·데드락·인덱스 위생 | 해당 화면 + 이벤트 | M6 |
| M9 | 알림 | 규칙 엔진 + Slack/Telegram/인앱 | 알림 발송·해소 | M8 |
| M10 | AI 어드바이저 | 규칙 엔진 + Bedrock + 검증 | 개선 가이드 | M7 |
| M11 | 리포팅 | 월간 + 개선 리포트 | 자동 생성 리포트 | M7, M10 |
| M12 | 확장·경화 | 샤딩·슬로우로그·부하 테스트·운영 문서 | 500대 준비 완료 | 전체 |

**MVP = M0~M6.** 여기까지가 "쓸 수 있는 도구"다.
**1차 정식 = M0~M11.**

병렬 가능: M7은 M5·M6과 독립(M4만 필요). M8·M9는 M6 이후 병렬. 프론트 작업은 M5부터
백엔드와 병렬 진행 가능(API 계약 우선 확정).

---

## M0. 기반

| # | 태스크 | 수용 기준 |
|---|---|---|
| ~~M0-1~~ | ~~Cargo 워크스페이스 생성 — **크레이트 4개** (`normalize`, `planparse`, `core`, `dbmon`). 나머지는 `dbmon` 안의 모듈 ([ADR-002](03-decisions.md))~~ → **완료** | `cargo build` 성공. 4개 크레이트, 각 테스트 있음 |
| ~~M0-2~~ | ~~`core` 도메인 타입 초안 (Instance, SlowQuery, Digest, Plan, Env, TimeRange)~~ → **완료** | 타입만. I/O 의존성 0 |
| ~~M0-2a~~ | ~~**`instance_id` 키 포맷 확정** ([OPEN-Q-20](OPEN-QUESTIONS.md))~~ → **완료** | **`<account>/<region>/<identifier>` 로 확정** (2026-08-19). `InstanceId::parse` 가 2성분을 거부한다 |
| ~~M0-3a~~ | ~~`core::ports::AuthTokenProvider` 분리 (MySQL 어댑터가 AWS를 모르게)~~ → **완료** | `dbmon::mysql` 이 AWS SDK 의존성을 갖지 않음 |
| ~~M0-3b~~ | ~~`core::ports::ArchiveQuery` 를 도메인 언어로 정의 (`QueryHandle`/`Cursor`/`AsOf`)~~ → **완료** | Athena 개념이 `core` 에 노출되지 않음 |
| ~~M0-3~~ | ~~`core` 포트 trait 정의 (SlowQueryStore, InstanceRegistry, TargetDb, MetricSource, Notifier, LlmAdvisor, ArchiveQuery, SecretSource, Clock)~~ → **완료** | trait 정의 + 페이크 구현 완료 |
| ~~M0-4~~ | ~~설정 로더 (환경변수 > 파일 > DynamoDB > 기본값). 기동 시 필수값 검증~~ → **완료** | 3단 병합(기본 문서 → 파일 → 환경변수). 교차 검증 포함 |
| ~~M0-5~~ | ~~로깅·트레이싱 (`tracing` JSON) + **마스킹 레이어**~~ → **완료** | `scrub`/`Scrubbed`/`sql_fingerprint` 단위 테스트 통과 |
| ~~M0-6~~ | ~~`Secret<T>` 타입 (Debug/Display/Serialize 미구현, Drop 시 zeroize)~~ → **완료** | 로깅 시도 시 컴파일 에러. `Drop` zeroize 확인 |
| ~~M0-7~~ | ~~에러 타입 체계 (`thiserror`) + API 에러 매핑~~ → **완료** | `DomainError` + `is_safe_to_expose` 정의 |
| ~~M0-8~~ | ~~`dbmon` 골격~~ → **완료.** 플래그·조립·그레이스풀 셧다운·`/healthz`·`/readyz` | SIGTERM 시 정상 종료 확인. `dbmon healthcheck` 서브커맨드로 컨테이너 헬스체크(이미지에 `curl` 불필요) |
| ~~M0-8a~~ | ~~ASG Lifecycle Hook 처리~~ → **불필요.** ECS 의 `stopTimeout` 이 대체한다 ([ADR-022](03-decisions.md)) | 해당 없음 |
| ~~M0-8b~~ | ~~자체 unhealthy 판정 (`SetInstanceHealth`)~~ → **불필요.** 컨테이너 헬스체크 실패 시 ECS 가 교체한다 | 해당 없음 |
| ~~M0-9~~ | ~~로컬 개발 환경 (docker-compose: MySQL 8.4, 8.0.32, DynamoDB Local)~~ → **완료** | `docker compose up -d` — MySQL 8.4 / 8.4-wide / 8.0 / DynamoDB Local |
| ~~M0-10~~ | ~~`just seed`: 로컬 MySQL에 샘플 스키마 + 슬로우 쿼리 생성기~~ → **완료** | `local/seed/*.sql` + `local/loadgen.sh` 시나리오 9종 |
| ~~M0-11~~ | ~~CI 파이프라인 ([14 §6.1](14-infrastructure.md)) — **arm64 러너**~~ → **완료** | **완료.** `.github/workflows/ci.yml` — 6개 잡, AWS 자격증명 불필요. arm64 러너로 이미지 빌드 |
| M0-11a | GitHub OIDC + CI Role (`dbmon-ci-dev` / `dbmon-ci-prd`) | 장기 액세스 키 미사용. `sub` 조건으로 레포·환경 제한 |
| M0-11b | 정적 검사 게이트 12종 ([15 §9.1](15-testing.md)) | 각 검사가 위반 코드를 실제로 잡아냄 |
| M0-12 | 프론트 스캐폴드 (Vite + React 19 + TS + Tailwind + shadcn/ui) | 빌드·lint·타입체크 통과 |
| ~~M0-13~~ | ~~Terraform 레이어 골격~~ → **완료.** 7개 레이어 전부 `terraform validate` 통과 ([infra/](../infra/README.md)) | `10-foundation`·`40-compute`·`60-seed` 는 실제 리소스 포함. `20`·`30`·`50` 은 골격 |
| **M0-13a** | `00-bootstrap` apply — state 백엔드 (S3 + DynamoDB 락) | `dbmon-tfstate-123456789012` 생성. 이후 레이어가 원격 state 사용 |
| **M0-13b** | `10-foundation` apply — DynamoDB·S3·KMS·DynamoDB 게이트웨이 엔드포인트. **기존 VPC는 data source로만 참조** | **로컬 앱이 실제 DynamoDB에 쓴다.** `terraform destroy`가 `dev-vpc-01`을 건드리지 않음을 `plan`으로 확인 |
| **M0-13c** | `60-seed` apply — 시드 MySQL 8.4 (`db.t4g.micro`) + 파라미터 그룹 ([18 §5](18-dev-environment.md)) | `performance_schema=ON`, `slow_query_log=ON`, IAM auth 활성. **모니터링 대상 확보** |
| **M0-13d** | 부하 생성기 (`just seed`) — 검증 시나리오 9종 ([18 §5.1](18-dev-environment.md)) | 1024바이트 초과 SQL, DML 장기 실행, 락 경합, 데드락, 다이제스트 1000종 생성 |
| **M0-13e** | `default_tags` + `prevent_destroy` 규약 ([14 §1](14-infrastructure.md)) | 전 리소스에 `Project=dbmon` 태그. KMS·DynamoDB·테이블버킷에 `prevent_destroy` |
| **M0-13g** | **Cognito 콜백 URL에 `http://localhost:8080/auth/callback` 등록 가능 여부 확인** ([18 §4](18-dev-environment.md)) | 등록 성공 → dev 접근 모델 확정. 실패 → ALB + ACM 폴백(+$16/월) |
| ~~M0-13f~~ | ~~탐색 필터 (`allowed_vpc_ids` + 태그 + 이름 거부, AND) — **prd 혼재 계정 격리** (T-37)~~ → **완료** | `deployment_env != prd` 면 `allowed_vpc_ids` 필수. Terraform 쪽도 plan 시점에 강제 |
| M0-14 | `docs/` ADR 템플릿 + 결정 로그 관리 규칙 | — |

**M0 완료 기준** — 빈 바이너리가 **컨테이너에서** 기동하고 `/healthz` 가 200 을 반환하며 CI 가 초록.

**현재 상태 (2026-08-19)**: 이미지 검증 완료(arm64, 154MB, healthy, SIGTERM 0초, PID 1 = dbmon).
CI 워크플로 작성 완료. 남은 것은 **M0-11a**(GitHub OIDC Role), **M0-11b**(정적 검사 12종),
**M0-12**(프론트 스캐폴드), 그리고 AWS apply 가 필요한 **M0-13a~g**.

---

## M1. 검증 스파이크 (매우 중요)

설계의 위험한 전제를 **코드로 확인**한다. 여기서 틀린 게 나오면 M2 이후 설계가 바뀐다.
각 스파이크는 **버리는 코드**여도 되지만, 결과는 반드시 문서에 반영한다.

| # | 태스크 | 검증 대상 | 실패 시 대응 |
|---|---|---|---|
| ~~M1-1~~ | ~~`information_schema.PROCESSLIST.INFO` 절단 여부~~ → **완료**. `varchar(21845)`, 65,535바이트 상한 ([19 §A](19-m1-findings.md)) | [OPEN-Q-07](OPEN-QUESTIONS.md) **해소** | 65KB 초과 시 `sql_text_truncated=true` |
| ~~M1-2~~ | ~~`EXPLAIN FORMAT=JSON FOR CONNECTION` 동작~~ → **완료. ADR-006 깨짐.** 정적 전역 권한 전체 필요 → RDS 불가 ([19 §B](19-m1-findings.md)) | ADR-006 **대체** | `rerun` 이 기본, DML 은 `rerun_as_select` |
| ~~M1-3~~ | ~~`EXPLAIN FORMAT=TREE FOR CONNECTION` 지원 여부~~ → **완료. 지원됨** (8.4.11). 단 `FOR CONNECTION` 자체가 RDS 불가 | [OPEN-Q-05](OPEN-QUESTIONS.md) **해소** | 재실행 경로에서 TREE 수집 |
| ~~M1-4~~ | ~~데이터 `SELECT` 권한 없이 `EXPLAIN FOR CONNECTION` 가능한가~~ → **완료. 질문 전제가 틀렸다** — `SELECT` 가 있어도 타인 커넥션은 불가 | [OPEN-Q-06](OPEN-QUESTIONS.md) **해소** | **권한 모드 B 고정. 모드 C 폐기** |
| M1-5 | IAM DB Auth: 활성화에 재부팅이 필요한가, 연결·토큰 갱신 동작 | [OPEN-Q-10](OPEN-QUESTIONS.md) / ADR-007 | UI 문구·절차 확정 |
| ~~M1-6~~ | ~~다이제스트 골든 코퍼스 생성 + 정규화 규칙 확정~~ → **완료. 131건 전량 수렴**, 규칙 11건 정정 ([19 §C](19-m1-findings.md)) | ADR-011 유지 | 코퍼스는 `tests/fixtures/digest_corpus.sql` |
| M1-7 | ~~S3 Tables 리전 가용성 + Athena 연동~~ **완료**. 남은 것: `MERGE INTO` / `DELETE` 동작·멱등성, 관리형 컴팩션, `s3tables:*` IAM 액션명 (`20-data` apply 후) | [OPEN-Q-03](OPEN-QUESTIONS.md) **대부분 해소** | 플레인 S3 Parquet + Glue로 전환 |
| M1-8 | DynamoDB 증분 내보내기 → 언네스팅 SQL 작성 (모든 타입) | [04 §4.3](04-data-model.md) | 배열·맵을 JSON 문자열로 저장하는 방침 확정 |
| M1-9 | Rust 수집 루프 벤치마크 — 모의 엔드포인트 **125 / 250 / 500개** × 다이제스트 500 / 2000행. A-08(워커 1대 500대) 판정에 500개 조건이 필수 | [OPEN-Q-01](OPEN-QUESTIONS.md) / ADR-001 / A-08 | 워커 수·주기 재산정. 125대도 못 커버하면 펜싱 토큰(M12-21)이 선행 조건 |
| ~~M1-10~~ | ~~크로스 리전 지연 측정~~ → **현재 불필요**. 타 리전 RDS 0개 ([18 §1.3](18-dev-environment.md)). 코드 경로는 만들되 검증은 M12로 연기 | [OPEN-Q-02](OPEN-QUESTIONS.md) | 해당 없음 |
| M1-11 | **다이제스트 스냅샷 실측** (시드 인스턴스 대상. M0-13c·M0-13d 선행): 실제 인스턴스에서 `digests_size` 사용량, 활성 다이제스트 수, `LAST_SEEN` 필터 적용 전후 응답 크기 | [05 §2.5.3](05-collector.md) / [16 §2.9](16-cost.md) | 필터·컬럼분리 설계 검증. 예상보다 크면 스냅샷 주기 상향 |
| ~~M1-12~~ | ~~플릿 능력 인벤토리~~ → **완료 (2026-08-19)**. 전 리전 RDS 0개 ([18 §1.3](18-dev-environment.md)) | [OPEN-Q-12](OPEN-QUESTIONS.md) **해소** | 8.4+ 고정이 업그레이드 프로젝트가 될 위험 없음. 시드를 8.4로 만든다 |
| ~~M1-13~~ | ~~`DIGEST_TEXT` 절단이 `DIGEST` 해시도 바꾸는가~~ → **완료. 바꾼다** → `app_digest` 필수 ([19 §E](19-m1-findings.md)) | [OPEN-Q-16](OPEN-QUESTIONS.md) **해소** | ADR-011 유지. `max_digest_length` 를 읽어야 한다 |
| M1-14 | IAM DB Auth 활성화 전후 대상 인스턴스 `FreeableMemory` 비교 (`db.t4g.small`) | [OPEN-Q-17](OPEN-QUESTIONS.md) / ADR-007 | 작은 인스턴스는 비밀번호 방식 기본으로 |
| M1-15 | Cognito access token에 `cognito:groups`를 넣는 방법 확인 (트리거 V1/V2, 실제 그룹 멤버십) | [OPEN-Q-18](OPEN-QUESTIONS.md) | `USER` 레코드만으로 인가하면 트리거 자체가 불필요할 수 있다 |
| ~~M1-16~~ | ~~`information_schema.PROCESSLIST WHERE ID = ?` 비용 측정~~ → **완료(부분). 타깃 조회가 PS 폴링보다 싸다**(0.8~0.9배, 스레드 88개) ([19 §F](19-m1-findings.md)) | ADR-005 대가 해소 | 수천 스레드 재측정은 M12-8 |
| ~~M1-17~~ | ~~`sys.innodb_lock_waits` 컬럼명·절단 실측~~ → **완료. 컬럼 14개 전부 존재**, `waiting_query` 절단 확인, 2차 조회로 전문 확보 ([19 §G](19-m1-findings.md)) | [05 §2.8](05-collector.md) 확인 | `metadata_locks` 는 M8-15 |

**M1 완료 기준** — [OPEN-QUESTIONS.md](OPEN-QUESTIONS.md)의 검증 항목이 모두 해소되고,
관련 ADR과 요구사항이 갱신되었다.

이 마일스톤을 건너뛰면 M4~M7에서 재작업이 발생한다. 여기에 2주를 쓰는 게 싸다.

---

## M2. 제어면 (RDS 탐색·레지스트리)

| # | 태스크 | 수용 기준 |
|---|---|---|
| M2-1 | `dbmon::aws`: 리전별 클라이언트 캐시 + 자격증명 공급자 체인 | 로컬 SSO / ECS Task Role 양쪽 동작 |
| M2-2 | Terraform: `dynamodb` 모듈 (테이블 2개, GSI, TTL, PITR, CMK) | `apply` 후 테이블 존재. TTL·PITR 활성 확인 |
| M2-3 | Terraform: `iam` 모듈 (정책 6개 분리, Instance Profile) | `dbmon-rds-modify`는 기본 미첨부 |
| ~~M2-4~~ | ~~`InstanceRegistry` DynamoDB 구현~~ → **완료** | DynamoDB Local 에 실제로 쓰고 읽는 통합 테스트 9건 (`it_store.rs`). AWS 자격증명 불필요 |
| M2-5 | RDS 탐색: `DescribeDBInstances`/`DescribeDBClusters` 리전 병렬 | 리전 1개 실패 시 나머지는 성공 |
| M2-6 | 엔진·상태·버전 하한 필터 (FR-DSC-11) | 8.0.31 인스턴스가 `unsupported_version` |
| M2-7 | 태그 → 환경 분류 + 매핑 설정 (FR-DSC-03) | 매핑표 기반. 태그 없으면 `unknown` |
| M2-8 | 환경 수동 오버라이드 (FR-DSC-04) | 재탐색 후에도 유지. 태그 불일치 표시 |
| M2-9 | Aurora 클러스터·멤버 관계 (FR-DSC-06) | 라이터/리더 구분, `cluster_id` 연결 |
| M2-10 | 삭제 처리: 2회 연속 미발견 시 `deleted_at` (FR-DSC-07) | 1회 API 실패로 삭제되지 않음 |
| M2-11 | 대기 중 유지보수·EOL 표시 (FR-DSC-09) | |
| M2-12 | 자가진단 프레임워크 + 점검 13종 ([05 §9](05-collector.md)) | 각 점검의 실패 케이스가 정확한 사유를 반환 |
| M2-13 | 스케줄러 + 리더 리스 (DynamoDB 조건부 쓰기) | 워커 2개 중 하나만 탐색 실행 (통합 테스트) |
| M2-14 | 설정 3계층 병합 + 30초 폴링 반영 (FR-OPS-05) | 재시작 없이 반영 확인 |
| M2-15 | API: `/api/instances*`, `/api/settings/aws`, `/api/settings/iam-diagnostics` | 계약 테스트 통과 |
| M2-16 | IAM 진단 ([08 §5.3](08-security-auth.md)) | 권한 하나를 빼면 진단이 그걸 잡아냄 |
| M2-17 | EMF 메트릭 기반 마련 + CloudWatch 로그 그룹 | 메트릭이 CloudWatch에 나타남 |
| M2-18 | 설정 값 검증 + 교차 검증 ([14 §8.9](14-infrastructure.md)) | 범위 밖 값 거부. `query_timeout > poll_interval` 경고 |
| M2-19 | 인스턴스 이름 변경 감지 (`dbi_resource_id` 기준) + `renamed_to/from` 연결 ([04 §1.3](04-data-model.md)) | 이름 변경 후 과거 데이터 조회 가능 |
| M2-20 | Aurora 라이터/리더 역할 변경 이벤트 기록 | 페일오버 시 `ROLE_CHANGE` 이벤트 |
| M2-21 | RDS 이벤트 수집 (`DescribeEvents`, 15분 주기) + 중복 제거 (FR-DSC-12) | 페일오버·파라미터 적용이 이벤트로 남음 |
| M2-22 | 대상 RDS 인증서 만료 감시 (`CertificateDetails.ValidTill`) (FR-DSC-13) | 90일 미만 경고 |
| M2-23 | 네트워크 접근 요청 상태 추적 (FR-DSC-15) | `requested` 상태에서 `unreachable` 알림 억제 |
| M2-24 | 전역 kill switch (`collection.enabled`) (FR-OPS-11) | admin이 즉시 전체 수집 중단 가능 |
| M2-25 | `JobHeartbeat` 메트릭 + 침묵 감지 알람 (`TreatMissingData=breaching`) (FR-OPS-10) | 잡을 강제로 멈춰서 알람 발생 확인 |

**M2 완료 기준** — 리전 2개 이상에서 인스턴스가 자동 탐색되고 prd/stg/dev로 분류되며,
각 인스턴스의 자가진단 결과를 API로 조회할 수 있다.

---

## M3. 부트스트랩

| # | 태스크 | 수용 기준 |
|---|---|---|
| M3-1 | `SecretSource`: RDS 관리형 마스터 시크릿 조회 | `SecretStatus=rotating`이면 재시도 |
| M3-2 | `SecretSource`: 사용자 지정 ARN (태그 조건 검증) | 임의 ARN 차단 |
| M3-3 | 수동 입력 경로 (`Secret<T>`, 동기 처리, 10분 만료) | 로그·응답·DB에 값 부재 (통합 테스트로 검증) |
| M3-4 | `dbmon::mysql`: 마스터 자격증명으로 임시 연결 (TLS + CA 검증) | 인증서 검증 비활성 옵션 없음 |
| M3-5 | 권한 조회·파싱 (`information_schema.*_PRIVILEGES` + `SHOW GRANTS`) | 권한 diff 정확 |
| M3-6 | 계획 생성 (`/api/bootstrap/plan`), dry-run | SQL 전문 포함. `plan_id` 5분 만료 |
| M3-7 | 실행 (`/api/bootstrap/apply`) + 타이핑 확인 (prd) | 확인 없으면 거부 |
| M3-8 | 멱등 실행: 부족한 GRANT만 추가, 초과분은 경고만 | 2회 실행 후 상태 동일 |
| M3-9 | 권한 모드 A/B 구현 + 스키마 화이트리스트 | 모드 B 기본값 |
| M3-10 | 비밀번호 폴백 + Secrets Manager 저장 + 로테이션 Lambda (`ALTER USER USER()`) | 로테이션 후 자동 재연결 |
| M3-11 | IAM DB Auth 토큰 발급 (SigV4, 리전별 서명) | 크로스 리전 인스턴스 접속 성공 |
| M3-12 | RDS CA 번들 임베드 + 만료 감시 | 90일 미만이면 경고 |
| M3-13 | 연결 풀 (인스턴스당 최대 6 (hot 2~4 + bulk 1~2, F7), 지터 있는 재생성) | 풀 전체 동시 재생성 없음 |
| M3-14 | 수동 부트스트랩 스크립트 생성 (`/api/bootstrap/manual-script`) | 복사해서 실행하면 동작 |
| M3-15 | 감사 로그 ([07 §4](07-credentials-bootstrap.md)) | 자격증명 값 부재 |
| M3-16 | Terraform: `secrets` 모듈 (로테이션 Lambda) | |
| M3-17 | 진행 상황 WebSocket 통지 (`bootstrap_progress`) | |

**M3 완료 기준** — 부트스트랩으로 IAM DB Auth 모니터링 계정이 생성되고,
이후 비밀번호 없이 대상 DB에 접속해 `SELECT 1`이 성공한다. 로그 전문 검사에서
자격증명이 발견되지 않는다.

---

## M4. 수집기

| # | 태스크 | 수용 기준 |
|---|---|---|
| M4-1 | `normalize` 크레이트 + 골든 코퍼스 테스트 (M1-6 결과 반영) | 프로퍼티 테스트 P1~P6 통과 |
| M4-2 | `app_digest` 계산 + `digest_algo_version` | 결정론적, 8192자 절단 |
| M4-3 | `planparse`: EXPLAIN JSON v1/v2 파싱, 정규화 트리, 지문, 참조 테이블 추출 | 픽스처 40개 통과. fuzz 무패닉 |
| M4-4 | `detect` 루프: `performance_schema.processlist` 폴링 | tick당 쿼리 1건 (정상 상태) |
| M4-5 | 심층 조회: `information_schema.PROCESSLIST` + `events_statements_current` | 1024자 초과 SQL 절단 없음 (**R1**) |
| M4-6 | in-flight 플랜 수집 + 실패 분류 + 폴백 | UPDATE 플랜 수집 성공 (**R3**) |
| M4-7 | in-flight 상태 머신 + 스레드 ID 재사용 방어 | 페이크 클록 테이블 주도 테스트 (**R5**) |
| M4-8 | 시작 시각 추정 (TIME / TIMER_WAIT) + 단위 변환 | 4초 쿼리 → `duration_ms ≈ 4000` (**R4**) |
| M4-9 | 진행 중 선행 저장 + 확정 갱신. **정규화·마스킹을 선행 저장 전에 수행** + `literal_policy_at_ms` 고정 | `masked` 인스턴스의 `in_flight` 레코드에 리터럴 부재 (**F2**, R41) |
| M4-9a | 고아 `in_flight` 정리 (스케줄러 리더, 5분 주기, `last_seen_at_ms` 기준) + 희소 GSI | 워커 급사 후 5분 내 `abandoned` 확정 (**F4**, R42) |
| M4-9b | `upsert_merged()` 단일 함수로 양방향 병합 (양쪽 경로가 같은 코드) | 슬로우로그 선착 후 캡처 도착 → 레코드 1건, 정확 지표 보존 (**F5**, R43) |
| M4-10 | `SlowQueryStore` DynamoDB 구현 (키 인코딩, `dur_bucket`, GSI) | 키 왕복 테스트 |
| M4-11 | 플랜 zstd 압축 + 300KB 초과 S3 오프로드 | 경계 크기 테스트 |
| M4-12 | 리터럴 정책 (full/masked/off) 적용 | 정책별 저장 내용 검증 |
| M4-13 | 제외 규칙 (스키마·계정·호스트·정규식) | 자기 자신 제외 확인 |
| M4-14 | `digest` 루프: **2단 쿼리** (지표 + `LAST_SEEN` 필터 / 텍스트 분리 조회) + 델타 + 리셋 감지 | 리셋 시 음수 델타 없음 (**R8**). 필터 없는 쿼리를 타입으로 차단 (**R21**) |
| M4-14a | 필터 기준 시각을 대상 DB의 `NOW(6)`로 관리 + 5초 안전 여유 + 실패 시 마지막 성공 시각 유지 | 스냅샷 지연·실패 후에도 구간이 이어짐 |
| M4-14b | `DIGEST → app_digest` 워커 캐시 + 신규만 텍스트 조회 | 안정 상태에서 2차 쿼리 실행 횟수 0에 근접 |
| M4-15 | 시간 누산기 + 정시 플러시 + 상위 N + `_other` | `_other` 총량 보존 (**R9**) |
| M4-16 | `mysql_digest ↔ app_digest` 매핑 학습 + 절단 처리 | 긴 쿼리 그룹 합류 (**R15**) |
| M4-17 | `DigestText` 사전 + `SLOWEST` 고정 샘플 + `ps_sample_text` | 조건부 갱신 정확 |
| M4-18 | 백오프·서킷 브레이커 + 실패 분류별 처리 | 실패 주입 테스트 |
| M4-19 | 폭주 방어 (LIMIT, 심층 조회 상한, 레이트 리밋, 큐 상한) | 초당 100건 폭주 시 워커 생존 |
| M4-20 | 쓰기 버퍼 + 재시도 + 드롭 메트릭 | `DroppedRecords` 노출 |
| ~~M4-21~~ | ~~샤드 리스 (64샤드, 조건부 쓰기) + **수집 리더 게이트** ([05 §7.1](05-collector.md))~~ → **리스·리더 게이트 완료** | `DynamoLeaseStore` + `target_shard_count`. 두 워커 경합·만료 인수·빼앗긴 리스 갱신 거부를 DynamoDB Local 로 검증(5건). 남은 것: 수집 루프에 배선 |
| M4-21a | 인스턴스 수집 상태 영속화 (`CSTATE`) + 샤드 인수 시 계승 | 서킷 오픈 상태가 인수 후에도 유지 (**F24**, R45) |
| M4-21b | 시계 오프셋 추정 (`SELECT NOW(6)`) + 시각 계산 보정 | 오프셋 3초 주입 → 병합·파티션 정상 (**F14**, R46) |
| M4-14c | 플러시 실패 시 hour 별 미완료 큐 재시도 | 배치 부분 실패 → 재시도 후 성공 (**F20**) |
| M4-15a | `_other` 를 "전체 델타 − 저장분"으로 계산 (임계값 미만 포함) + `total_time_all_ms`·`top_n`·`partial_minutes` 기록 | `상위 N + _other = total_time_all_ms` (**F6**, **F21**, R9) |
| ~~M4-22~~ | ~~부하 자기 계측 (`/* dbmon: */` 주석 기반)~~ → **성립하지 않는다.** MySQL 다이제스트는 주석을 제거하므로 우리 쿼리를 주석으로 식별할 수 없다 (C-23). M4-29 의 계정 기반 방식을 쓴다 | 우리 쿼리 비중 산출 |
| M4-23 | 그레이스풀 셧다운 (리스 반납 → 누산기 플러시 → 버퍼 플러시) | 순서 검증 |
| M4-25 | `plan_normalized` 생성 (플랜 조건식 리터럴 마스킹) (FR-PLN-09) | `masked` 인스턴스의 `plan_normalized`에 리터럴 0개 (**R28**) |
| M4-26 | 제외 규칙에 걸린 세션의 플랜 미수집 (FR-PLN-10) | 제외 계정 세션의 플랜이 저장되지 않음 |
| M4-27 | 마스킹 후조건 검증 + 실패 시 강등 ([05 §3.4](05-collector.md)) | 리터럴 잔존 시 저장 안 함 + 카운터 |
| M4-28 | 계정별 시간 롤업 (`UR#`) + `seen_users`/`seen_hosts` ([ADR-021](03-decisions.md)) | 계정별 집계 조회 가능 |
| M4-29 | `sys.user_summary_by_statement_type` 기반 자기 부하 계측 (주석 대신 계정으로 식별) | 우리 쿼리 비중 산출 (**R34**) |
| M4-30 | 다이제스트 히스토그램 온디맨드 조회 (`events_statements_histogram_by_digest`, `WHERE DIGEST=?`) (FR-DGS-02b) | 시간창 P95 정확 계산 |
| M4-24 | E2E 통합 테스트 ([15 §4.1](15-testing.md)) | 전 항목 통과 |

**M4 완료 기준** — 로컬 MySQL 8.4에서 2초 이상 걸리는 SELECT/UPDATE에 대해
전문 SQL과 in-flight 실행계획이 DynamoDB에 저장되고, 다이제스트 시간 롤업이
`_other` 포함해 정확히 집계된다.

---

## M5. 인증·웹 셸

| # | 태스크 | 수용 기준 |
|---|---|---|
| M5-1 | Terraform: `cognito` 모듈 (User Pool, App Client, Domain, Groups, Pre-Token Lambda) | 자가 가입 비활성 확인 |
| M5-1a | 초기 admin 사용자 생성 (`var.initial_admin_email` 필수, 기본값 없음) | 값 없으면 `apply` 실패. 초대 메일로 첫 로그인 성공 |
| M5-2 | JWT 검증 미들웨어 + JWKS 캐시·회전. 검증 항목 `iss`/`exp`/`token_use`/**`client_id`**(aud 없음) | `kid` 미스 시 1회 갱신, 레이트 리밋 |
| M5-2a | **토큰 클레임 ∩ 서버 `USER` 레코드** 교집합 인가 (FR-AUT-08) | 위조 `cognito:groups`로 admin 주장 → 차단 (**T-20**) |
| M5-2b | 토큰 폐기 (`revoked_after_ms`, `claims_version`) (FR-AUT-12) | 그룹 강등 후 기존 토큰 → 401 (**T-33**) |
| M5-2c | HMAC 서명 커서 + 핫/콜드 `phase` ([13 §1.1](13-api-spec.md)) | 위조·타인 커서 → 400 (**T-32**) |
| M5-2d | Athena 쿼리 타입별 권한 + 결과 마스킹·스코프 후처리 ([13 §2.9](13-api-spec.md)) | viewer의 `audit_search` → 403 (**T-17**) |
| M5-2e | 감사 로그 해시 체인 + 외부 앵커 + IAM Deny (NFR-S-12) | 중간 레코드 제거 시 체인 검증 실패 (**T-21**) |
| M5-3 | `AuthContext` 추출자 (`Authed` / `OperatorOnly` / `AdminOnly`) | 라우터 순회 계약 테스트 |
| M5-4 | 환경 스코프 강제 (요청 파라미터 ∩ 사용자 스코프) | 스코프 밖 조회 시 빈 결과 |
| M5-5 | 리터럴 마스킹 (`Redactable` + 응답 래퍼) | `can_see_literals=false` 응답 검증 |
| M5-6 | 감사 로그 기록 + 수정·삭제 불가 IAM 조건 | |
| M5-7 | 프론트: OIDC PKCE 로그인·로그아웃·토큰 갱신 | 만료 시 자동 갱신 |
| M5-8 | 프론트: `/api/auth/config` 기반 IdP 버튼 동적 렌더 | 프론트 재빌드 없이 IdP 추가 |
| M5-9 | 프론트: 앱 셸 (사이드바, 헤더, 다크/라이트, 환경 색 체계) | |
| M5-10 | OpenAPI 생성 (`utoipa`) + 프론트 타입 생성 + CI diff 게이트 | 스펙 변경 시 CI 실패 |
| M5-11 | API: `/api/slow-queries*` 목록·상세·플랜 | 커서 페이지네이션 왕복 |
| M5-12 | API: `/api/digests*` 목록·상세·시계열·**samples** | `/samples` 응답이 4종 `kind` 반환 |
| M5-13 | 프론트: 슬로우 쿼리 목록 (가상 스크롤, 필터 → URL) | 1만 행 60fps |
| M5-14 | 프론트: 슬로우 쿼리 상세 (SQL 포맷, 플랜 트리/TREE/JSON, 출처 배지) | 플랜 출처 명확 표시 |
| M5-15 | 프론트: 다이제스트 목록 (정렬·필터·배지) | |
| M5-16 | 프론트: **다이제스트 상세** ([09 §3.1](09-frontend.md)) | 샘플 4종 출처 구분, 복사, 플랜 링크 |
| M5-17 | 프론트: 에러·빈 상태 전량 ([09 §8](09-frontend.md)) | |
| M5-18 | Terraform: `40-compute` (ECR, ECS Fargate, 조건부 ALB) + 프론트 임베드 배포 | **골격 완료.** dev 는 `enable_alb=false` 로 네트워크 비용 $0 |
| M5-19 | E2E S1, S2, S3, S6 | S3(다이제스트 → 샘플 복사) 통과 |

**M5 완료 기준** — Cognito 로그인 없이는 어떤 데이터도 조회되지 않고, 로그인 후
다이제스트 상세에서 집계 통계 + 실제 샘플 쿼리 + 플랜을 볼 수 있다.
viewer 역할로는 변경 API가 403.

---

## M6. 메트릭

| # | 태스크 | 수용 기준 |
|---|---|---|
| M6-1 | `status` 루프: `global_status` 델타 + 파생 지표 | Uptime 감소 시 델타 스킵 |
| M6-2 | 실시간 링버퍼 (인스턴스당 60분) | 메모리 사용량 측정 |
| M6-3 | WebSocket 서버 (인증·구독·인가·하트비트·백프레셔) | 미인증 5초 후 종료 |
| M6-4 | WS 토픽 구현 (`slowq:*`, `status:*`, `alert:*`, `job:*`, `query:*`) | 환경 스코프 검증 |
| M6-5 | `status:env=*` 서버 측 집계 (500대 개별 구독 방지) | |
| M6-6 | `MetricSource` CloudWatch 구현 (`GetMetricData` 배치) | `GetMetricStatistics` 미사용 |
| M6-7 | 엔진별 메트릭 카탈로그 (MySQL / Aurora / Serverless v2) | |
| M6-8 | period 자동 선택 + 보관 기간 제약 대응 | 조정 시 응답에 표시 |
| M6-9 | 캐시 (period 정렬 키) + 예산 상한·캐시 강제 모드 | 캐시 히트 시 API 호출 0 (**R19**) |
| M6-10 | 플릿 폴링 (3메트릭 × 15분) | 월 요청 수 예산 내 |
| M6-11 | `MetricRollup` 시간 롤업 저장 (선별 지표) | |
| M6-12 | 사용량 집계 잡 + `/api/usage` | 일별 추이 노출 |
| M6-13 | 프론트: `<TimeSeriesChart>` (uPlot 래퍼) | 4만 포인트 60fps |
| M6-14 | 프론트: HTML/CSS 차트 (`<BarList>`, `<Sparkline>`, `<RatioBar>`) | 의존성 0. Recharts 미도입 ([ADR-013](03-decisions.md)) |
| M6-15 | 프론트: WS 훅 (재연결, 구독 복원, 16ms 배칭) | 모의 WS 테스트 |
| M6-16 | 프론트: 플릿 개요 (가상 그리드, 공유 canvas 스파크라인) | 500 카드 렌더 |
| M6-17 | 프론트: 인스턴스 상세 + 메트릭 탭 (실시간/CloudWatch 시각 분리) | 두 영역 명확 구분 |
| M6-18 | 프론트: 실시간 스트림 화면 | 서버 측 필터 적용 |
| M6-19 | 슬로우 쿼리 오버레이 (FR-MET-07) | 차트 위 마커 |
| M6-20 | 프론트: 자가진단 탭 | 미충족 항목 + 해결 방법 |

**M6 완료 기준** — 플릿 개요가 자체 수집 지표로 실시간 렌더되고(CloudWatch 미사용),
인스턴스 상세에서 CloudWatch 추세를 볼 수 있으며 API 예산 사용량이 노출된다.

---

## M7. 아카이브

| # | 태스크 | 수용 기준 |
|---|---|---|
| M7-1 | Terraform: `storage` 모듈 (S3 버킷 6개 + S3 Tables 네임스페이스·테이블 9개) | Iceberg DDL 적용 |
| M7-2 | Terraform: `athena` 모듈 (워크그룹, 스캔 한도, Glue DB, 결과 버킷) | 쿼리당 10GB 한도 |
| M7-3 | 아카이브 잡: 증분 내보내기 시작·폴링 + 체크포인트 | 실패 시 체크포인트 유지 |
| M7-4 | raw 외부 테이블 + 언네스팅 뷰 (M1-8 결과 반영) | 전 타입 매핑 정확 |
| M7-5 | `MERGE INTO` 적재 (테이블 9개) | 2회 실행 후 행수 불변 (**R14**) |
| M7-6 | 삭제 마커 필터 | TTL 만료가 아카이브에 반영 안 됨 |
| M7-6a | 체크포인트를 테이블별로 분리 (`CKPT/archive#<table>`) + PITR 창 접근 알람 | 테이블 1개 실패가 나머지를 막지 않음 (**F11**) |
| M7-6b | Iceberg 스키마 변경 3단계 절차 + 잡 시작 시 `schema_version` 검증 | 컬럼 불일치 시 조기 실패 (**F26**) |
| M7-7 | 보관 만료 잡 (월 1회 `DELETE`) + 스냅샷 만료 확인 | 스토리지 실제 감소 |
| M7-8 | `ArchiveQuery` Athena 구현 (비동기, single-flight). **자체 캐시 대신 워크그룹 result reuse** ([ADR-015](03-decisions.md)) | 동시 동일 쿼리 1회 실행. `QueryExecutionId`를 `sub`에 바인딩 |
| M7-9 | 조회 라우팅 (`QueryRouter`) + 경계 교차 dedup 병합 | 중복·누락 없음 |
| M7-10 | 쿼리 템플릿 enum + `TimeRange` 필수 인자 + 파라미터화 | 문자열 결합 없음 (**R17**) |
| M7-11 | API: `/api/queries*` (시작·상태·결과·취소) | |
| M7-12 | ~~WS `athena_done` 통지~~ → **제거.** HTTP 폴링으로 대체 (ADR-015) | 해당 없음 |
| M7-13 | 프론트: 비동기 조회 UX (진행 시간, 스캔량, 취소) | |
| M7-14 | 아카이브 경로 통합 테스트 (dev AWS, 일 1회 CI) | 전 항목 통과 |
| M7-15 | S3 오프로드된 플랜의 Iceberg 경유 복원 | 35일 이전 플랜 조회 가능 |

**M7 완료 기준** — 32일 전 슬로우 쿼리가 Athena 경로로 조회되고, 아카이브 잡을
2회 연속 실행해도 Iceberg 행수가 변하지 않는다.

---

## M8. 확장 관측

| # | 태스크 | 수용 기준 |
|---|---|---|
| M8-1 | `health` 루프: `SHOW REPLICA STATUS` 파싱 | 0행(복제 미구성)을 에러로 취급 안 함 |
| M8-2 | Aurora `replica_host_status` | |
| M8-3 | `SHOW BINARY LOG STATUS` (8.4) / `SHOW MASTER STATUS` (8.0) 버전 분기 | |
| M8-4 | 락 대기 (`sys.innodb_lock_waits`) + 대기 체인 구성 | 의도적 락 경합 통합 테스트 |
| M8-5 | 장기 트랜잭션 (`innodb_trx`) | |
| M8-6 | 데드락 (`SHOW ENGINE INNODB STATUS` 파싱, 5분 주기, 중복 병합) | 같은 데드락 1건으로 병합 |
| M8-7 | 인덱스 위생 (`sys.schema_unused_indexes`, `schema_redundant_indexes`) | uptime < 7일 → `low_confidence` (**R10**) |
| M8-8 | AUTO_INCREMENT 고갈 (`sys.schema_auto_increment_columns`) | |
| M8-9 | 테이블 통계 스냅샷 + 증가 추이 | "추정치" 표기 (**R11**) |
| M8-10 | 스키마 지문 스냅샷 + 변경 이력 | |
| M8-11 | 파라미터 스냅샷 + 기준선 diff | |
| M8-12 | `Event` 저장 + `/api/events` | |
| M8-20 | 조용한 0데이터 감지 (`Queries` 델타 × digest 행 수 조합) ([05 §9.1](05-collector.md)) | consumer 를 런타임에 끄면 5분 내 `degraded` (**F15**, R47) |
| M8-13 | 프론트: 복제·락·인덱스 위생 탭 | 블로킹 체인 트리 시각화 |
| M8-14 | 프론트: 락 화면에 `KILL` 명령 **표시만** | 실행 버튼 없음 |
| M8-15 | **메타데이터 락(MDL) 대기** (`performance_schema.metadata_locks`) (FR-OBS-12) | ALTER TABLE 블로킹 시나리오 통합 테스트 |
| M8-16 | **InnoDB 퍼지 지연** (`INNODB_METRICS.trx_rseg_history_len`) (FR-OBS-13) | RDS MySQL에서 값 수집 |
| M8-17 | 락 화면 전문 SQL을 `information_schema.PROCESSLIST` 2차 조회로 (sys 뷰 64자 절단 회피) | SQL이 64자를 넘고 `...`로 끝나지 않음 (**R29**) |
| M8-18 | 파티션 관리 감시 (`information_schema.PARTITIONS`) (FR-OBS-15) | P2 |
| M8-19 | 콜레이션 불일치 탐지 (FR-OBS-16) | P2 |

**M8 완료 기준** — 복제 지연·락 대기·데드락·인덱스 위생이 수집되어 화면에 표시되고,
`sys.schema_unused_indexes`의 재시작 직후 오탐이 방지된다.

---

## M9. 알림

| # | 태스크 | 수용 기준 |
|---|---|---|
| M9-1 | 규칙 모델 + 저장 + 검증 | 스코프 미리보기 |
| M9-2 | 평가 엔진 (소스별 어댑터) | 소스 9종 |
| M9-3 | 상태 머신 (pending/firing/resolved, `resolve_after`) | 테이블 주도 테스트, 플래핑 흡수 |
| M9-3a | **평가(collector)와 발송(control) 분리** + DynamoDB 조건부 전이 + 발송 의도 큐 ([10 §2.0](10-alerting.md)) | 워커 2개가 동시 평가해도 발화 1회 (통합 테스트) |
| M9-3b | `dbmon-config` 일 1회 JSON 백업 (아카이브 잡에 편승) — [OPEN-Q-14](OPEN-QUESTIONS.md) | 백업에서 규칙·채널 복원 가능 |
| M9-4 | `spike_ratio` 계산 (7일 같은 시간대 중앙값) | 데이터 부족 시 평가 스킵 |
| M9-5 | 중복 억제·그룹핑·재통지 | 60초 내 다중 지문 → 1건 |
| M9-6 | 음소거·점검 창 | cron 표현식 파싱 |
| M9-7 | 의존 억제 (collector·platform 우선) | 수집 실패 중 억제 (**R20**) |
| M9-8 | 발송 레이트 리밋 (전역·규칙·채널) | 토큰 버킷 경계 테스트 |
| M9-9 | Slack 어댑터 (Webhook + Bot, Block Kit, 해소 시 원본 수정) | |
| M9-10 | Telegram 어댑터 (HTML 이스케이프, 4096자 절단, `editMessageText`) | `<script>` 안전 렌더 |
| M9-11 | 인앱 채널 (`AlertEvent` + WS) | |
| M9-12 | 채널 자격증명 Secrets Manager 저장 + 등록 시 테스트 발송 | UI 재표시 없음 |
| M9-13 | 웹훅 URL 검증 (도메인 허용목록, 사설 IP, DNS 리바인딩) | SSRF 테스트 통과 (**R18**) |
| M9-14 | 발송 재시도 + 채널 자동 비활성 | |
| M9-15 | 기본 규칙 템플릿 23종 (비활성 상태로 생성) | |
| M9-16 | 딥링크 생성 (기간·인스턴스 포함) | 클릭 시 해당 시각으로 이동 |
| M9-20 | **알림 본문에서 리터럴 제거** (`digest_text`만) (T-23) | 발송 페이로드 검사 (**R39**) |
| M9-21 | `plan_change` 알림 소스 (접근 방식 악화만 발화) | 플랜 지문 변경 → 알림 |
| M9-22 | `rds_event` 알림 소스 (페일오버·장애·파라미터 적용) | RDS 이벤트 → 알림 |
| M9-23 | `certificate` 알림 소스 (대상 RDS 인증서 만료) | 30일 미만 → critical |
| M9-24 | egress 인벤토리 문서화 + 각 경로 감사 이벤트 (NFR-S-13) | 6개 경로 전부 감사 이벤트 존재 |
| M9-17 | API: `/api/alerts*`, `/api/alert-rules*`, `/api/channels*`, `/api/mutes*` | |
| M9-18 | 프론트: 알림 센터 + 규칙 CRUD + 채널 설정 | 규칙별 발화 통계·튜닝 배지 |
| M9-19 | E2E S5 | |

**M9 완료 기준** — Slack/Telegram으로 알림이 발송·해소되고, 중복 억제와 의존 억제가
동작하며, 규칙별 발화 통계가 화면에 표시된다.

---

## M10. AI 어드바이저

| # | 태스크 | 수용 기준 |
|---|---|---|
| M10-1 | Terraform: `bedrock` 모듈 (Guardrail) + `dbmon-ai` IAM 정책 | |
| M10-2 | 참조 테이블 추출 (플랜 우선, SQL 파서 폴백) | 픽스처 30개 |
| M10-3 | 스키마 컨텍스트 수집 (`SHOW CREATE TABLE`, STATISTICS, TABLES, COLUMN_STATISTICS) | 테이블 10개 상한, 15분 캐시 |
| M10-4 | `schema_fingerprint` 계산 | 인덱스 변경 → 변경, 행수 변경 → 불변 |
| M10-5 | **결정론적 규칙 엔진 16종** ([11 §4](11-ai-advisor.md)) | 테이블 주도 테스트 |
| M10-6 | 프롬프트 파일 관리 (`prompts/advisor/v*.toml`) + 버전 해시 | 재컴파일 없이 교체 |
| M10-7 | Bedrock Converse 호출 + tool-use 스키마 강제 + 프롬프트 캐싱 | |
| M10-8 | 리터럴 미전송 (골든 페이로드 테스트) | 스냅샷에 리터럴 부재 (**R13**) |
| M10-9 | 응답 검증: 스키마 → 식별자 → DDL 안전성 → 수치 | 우회 시도 전량 차단 |
| M10-10 | 캐시 (4축 키) + 히트율 메트릭 | 축별 히트/미스 테스트 |
| M10-11 | 예산 통제 (환경별 월 토큰, 80%/100% 동작) | 경계 테스트 |
| M10-12 | 자동 실행 조건 + 인스턴스당 시간당 3건 제한 | |
| M10-13 | 피드백 저장 (applied/deferred/rejected) | |
| M10-14 | API: `/api/digests/{d}/advice`(GET/POST), `/tables`, `/advice/feedback` | |
| M10-15 | 프론트: 어드바이저 결과 ("사실" vs "AI" 분리, hallucinated·미검증 수치 표시) | |
| M10-16 | 실패 처리 전량 ([11 §9](11-ai-advisor.md)) | 규칙 엔진 결과만이라도 표시 |
| M10-17 | Bedrock Guardrail IAM 조건 강제 (`bedrock:GuardrailIdentifier`) (T-31) | 조건 없는 호출 → AccessDenied |
| M10-18 | 프롬프트 인젝션 방어 4중 (`untrusted_db_metadata` 구분, 패턴 중화, `drop_index` 옵트인, 사용 통계 교차 검증) ([11 §6.1](11-ai-advisor.md)) | COMMENT 지시문 픽스처 → 차단 (**T-34**) |
| M10-19 | 응답 스키마 앱 측 검증 + `toolResult status=error` 재요청 (최대 2회) | Bedrock이 검증해 주지 않으므로 필수 |
| M10-20 | `STALE_STATS` 규칙을 `mysql.innodb_table_stats` 기반으로 (권한 모드 A만) | 권한 없으면 규칙 미발화 (근거 없는 사실 금지) |

**M10 완료 기준** — 다이제스트에서 어드바이저를 실행하면 확정된 사실 + 인덱스 권고 DDL +
리스크 + 검증 방법이 구조화되어 표시되고, 리터럴이 전송되지 않으며, 존재하지 않는
식별자를 참조하는 권고가 경고 표시된다.

---

## M11. 리포팅

| # | 태스크 | 수용 기준 |
|---|---|---|
| M11-1 | 집계 쿼리 파일 15종 (`queries/reports/monthly/*.sql`) + 해시 관리 | 파티션 프루닝 정적 검사 |
| M11-1a | 리포트 시간대 경계 계산 (`report_timezone`, 기본 Asia/Seoul) ([12 §0.1](12-reporting.md)) | KST 7월 = UTC 6/30 15:00 ~ 7/31 15:00. 메타에 UTC 경계 기록 (**R22**) |
| M11-2 | 월간 리포트 파이프라인 (Athena → JSON → CloudWatch → Bedrock → HTML → S3) | 환경당 10분 이내 |
| M11-3 | 관측 커버리지 계산 (`_other` 기반) | |
| M11-4 | 부분 실패 허용 + 실패 섹션 표시 | 섹션 1개 실패 시 리포트 생성됨 |
| M11-5 | 수치 검증기 (허용 집합 + 숫자 추출 + 재시도) | 조작 서술문 검출 |
| M11-6 | HTML 템플릿 (`minijinja`) + 인쇄 CSS + 정적 SVG 차트 | JS 없이 열림, A4 인쇄 |
| M11-7 | 개선 리포트: 기간 비교 + 건당 정규화 + 판정 5등급 | 표본 30건 미만 → "판정 불가" |
| M11-8 | 플랜 diff (before/after 대표 플랜) | Iceberg에서 플랜 복원 |
| M11-9 | 개선 리포트 서술 (인과 단정 금지 프롬프트) | |
| M11-10 | 리포트 메타 + 재현성 필드 | 같은 기간 2회 생성 → 수치 동일 |
| M11-11 | EventBridge Scheduler 등록 (매월 1일) | |
| M11-12 | API: `/api/reports*` | |
| M11-13 | 프론트: 리포트 목록·조회 (presigned URL) | |
| M11-14 | Slack/Telegram 요약 발송 (FR-RPT-07) — **앱 프록시 링크** | presigned URL 미포함 |
| M11-15 | **typed template** 수치 문장 생성 (섹션별 2~4문장) ([12 §4](12-reporting.md)) | 수치가 코드로 바인딩됨 |
| M11-16 | LLM 응답 숫자 0개 검증 + 위반 문장 제거 | 숫자 포함 응답 → 문장 제거 (**R37**) |
| M11-17 | Iceberg 스냅샷 ID 고정 + `FOR VERSION AS OF` 재현 (FR-RPT-08) | 같은 스냅샷으로 재생성 시 수치 동일 |
| M11-18 | 리포트 앱 프록시 (`/render`) + CSP 헤더 + 감사 (FR-RPT-09) | presigned 직접 노출 부재 (**R38**) |
| M11-19 | `report_timezone` 경계 계산 + 메타 기록 (FR-RPT-10) | KST 7월 = UTC 6/30 15:00 ~ 7/31 15:00 (**R22**) |
| M11-20 | 아카이브·리포트 전용 워크그룹 분리 + 사용자별 스캔 예산 (T-35) | viewer가 한도 소진해도 리포트 정상 |

**M11 완료 기준** — 월간 리포트가 자동 생성되고 모든 수치가 Athena 결과와 일치하며,
개선 리포트가 인덱스 추가 전후를 건당 지표로 비교해 판정한다.

---

## M12. 확장·경화

| # | 태스크 | 수용 기준 |
|---|---|---|
| M12-1 | 슬로우로그 파서 + `FilterLogEvents` 수집 | 실제 RDS 로그 샘플 |
| M12-2 | 3소스 병합 로직 | 같은 실행이 1건으로 |
| M12-3 | 백필 (기간 지정, 인스턴스 병렬, 체크포인트) | 중단 후 재개 |
| M12-4 | 31일 초과 백필의 Iceberg 직접 적재 경로 + **예외 규칙 6개** ([05 §8.3](05-collector.md)) | `capture_source=backfill`, 영향 리포트 `stale` 표시 (**F12**) |
| M12-5 | 역할 분리 배포 (`--role api|collector|control`) | 2단계 토폴로지 |
| M12-6 | **WS 팬아웃** (워커 간 이벤트 전파) — [OPEN-Q-04](OPEN-QUESTIONS.md) 결론 반영 | 워커 3대에서 실시간 데이터 정상 |
| M12-7 | 부하 테스트 하네스 (모의 MySQL 엔드포인트) | 500개 엔드포인트 |
| M12-8 | 부하 테스트 실행 + NFR-P-01/02 검증 | [15 §8.3](15-testing.md) 기준 통과 |
| M12-9 | 대상 DB 부하 실측 (sysbench 병행) | CPU 증가 < 1% |
| M12-10 | 샤드 불균형 대응 (가중 할당) — [OPEN-Q-09](OPEN-QUESTIONS.md) | |
| M12-11 | Enhanced Monitoring (OS 지표) — FR-MET-08 | |
| M12-12 | SQL 텍스트 검색 (Athena 경로) — [OPEN-Q-08](OPEN-QUESTIONS.md) | |
| M12-13 | 보안 검증 항목 전량 실행 ([08 §11](08-security-auth.md)) | 전 항목 통과 |
| M12-14 | 회귀 테스트 대장 **R1~R40** 전량 구현 확인 | 40건 전부 대응 테스트 존재 |
| M12-15 | 접근성 검증 (axe-core + 키보드 전용 시나리오) | |
| M12-16 | 운영 런북 검증 (각 시나리오를 의도적으로 유발해 절차 확인) | |
| M12-17 | 비용 실측 → [16-cost.md](16-cost.md) 갱신 | |
| M12-18 | 사용자 문서 (설치 가이드, 운영 가이드, 트러블슈팅) | 새 사람이 문서만 보고 설치 성공 |
| M12-19 | `dbmon-config` lazy 마이그레이션 (`v` 기반) ([14 §6.5](14-infrastructure.md)) | 구 버전 항목이 읽기 시 변환·재저장됨 |
| M12-20 | 재해 복구 절차 리허설 ([14 §8.8](14-infrastructure.md)) | PITR 복원 → 테이블 교체 → 재기동 실제 수행 (연 1회) |
| M12-21 | **펜싱 토큰** (샤드별 단조 증가 epoch + 모든 쓰기 조건식) ([ADR-018](03-decisions.md)) | 워커 2개 동시 소유 시뮬레이션 → 이전 소유자 쓰기 거부 |
| M12-22 | 고갈 예측 화면 (디스크 ETA, AUTO_INCREMENT 소진일, 테이블 증가 추이) | P2 |
| M12-23 | 테이블 중심 집계 화면 (`referenced_tables` 롤업) | P2 |
| M12-24 | 실행계획 마크다운 내보내기 (1세대 기능 복원) | P2 |
| M12-25 | 수동 플랜 재수집 트리거 (`plan_source=none` 레코드) | P2 |
| M12-26 | 헤더에 AWS 계정·리전 배지 (1세대 기능) | P2 |
| **M12-27** | dev에 ALB 레이어를 임시 apply → **ALB 헬스체크·등록해제·WS 업그레이드·WAF 규칙·엔드포인트 IAM 조건** 검증 → destroy ([OPEN-Q-21](OPEN-QUESTIONS.md)) | dev 토폴로지에서 검증 불가한 4개 항목 확인. 비용 $1 미만 |
| **M12-28** | prd 첫 배포를 **수집 대상 0대**로 띄워 프라이빗 서브넷 DNS·NAT 경유·`AF_NETLINK` 하드닝 확인 후 인스턴스 연결 | prd에서 처음 실패를 보지 않는다 |
| **M12-29** | 크로스 리전 대상이 실제로 생기면 지연·전송 비용 측정 ([OPEN-Q-02](OPEN-QUESTIONS.md)) | 조건부 태스크 |

---

## 부록 A. 미할당 백로그 (P2)

우선순위가 낮거나 요구가 확실해진 뒤에 하는 것.

| 항목 | 조건 |
|---|---|
| 풀스캔 쿼리 목록 (`sys.statements_with_full_table_scans`) | 인덱스 위생 화면 사용률 확인 후 |
| Slack 상호작용 버튼 (음소거·확인) | Slack 앱 설치 정책 확인 후 |
| 리포트 PDF 직접 생성 | 브라우저 인쇄가 부족하다는 피드백 후 |
| 다이제스트 태그·주석 (담당자 지정, 조사 메모) | 팀 협업 요구 발생 후 |
| 슬로우 쿼리 CSV·JSON 내보내기 | 감사 로그 연동 필수 |
| PostgreSQL 지원 | 대상 DB에 PostgreSQL이 실제로 생긴 후 |
| 멀티 계정 (AssumeRole) | 계정 분리가 실제로 발생한 후 |
| 쿼리 KILL 기능 | [OPEN-Q-11](OPEN-QUESTIONS.md) 결정 후 |
| Performance Insights 연동 | PI가 켜진 인스턴스 비중 확인 후 |
| 배포 이벤트 상관관계 (CI 웹훅 수신) | 배포 파이프라인 연동 여력 확인 후 |
| 이상 탐지 (통계적 베이스라인) | 알림 규칙만으로 부족하다는 피드백 후 |

## 부록 B. 태스크 작성 규칙

새 태스크를 추가할 때:
1. **수용 기준을 반드시 쓴다.** "구현한다"는 수용 기준이 아니다. 무엇을 확인하면
   끝난 것인지 쓴다.
2. **요구사항 ID 또는 회귀 번호를 연결한다.** 어디서 온 태스크인지 추적 가능하게.
3. **테스트를 별도 태스크로 만들지 않는다.** 수용 기준에 포함한다.
   테스트를 뒤로 미룰 수 있게 만들면 미뤄진다.
4. **문서 갱신도 수용 기준에 넣는다.** 특히 M1의 검증 결과와 M12의 비용 실측.

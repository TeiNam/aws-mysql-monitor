# 06. RDS 탐색과 메트릭

## 1. 인스턴스 자동 탐색

### 1.1 수집 대상 API

```
리전별 병렬 실행:
  rds:DescribeDBInstances    (paginator)   → 인스턴스 + TagList
  rds:DescribeDBClusters     (paginator)   → Aurora 클러스터 + 멤버 구성
  rds:DescribePendingMaintenanceActions    → 대기 중 유지보수 (FR-DSC-09)
```

`DescribeDBInstances`의 `TagList`는 기본 응답에 포함되므로 `ListTagsForResource`를
인스턴스마다 따로 호출하지 않는다. (1세대는 `ListTagsForResource`를 별도 호출했고,
인스턴스가 많아지면 API 호출이 선형으로 늘어난다.)

### 1.2 필터

```
1. Engine ∈ {mysql, aurora-mysql}
2. DBInstanceStatus ∈ {available, backing-up, modifying, storage-optimization,
                       configuring-enhanced-monitoring}
   (creating, deleting, failed 등은 제외하되 목록에는 표시)
3. 엔진 버전 하한 판정 (문자열 비교 불가 — [ADR-019](03-decisions.md))
     mysql        : EngineVersion 을 semver 파싱 → >= 8.4.0
     aurora-mysql : EngineVersion 이 "8.0.mysql_aurora.3.05.2" 형태라 순서 비교 불가.
                    Aurora 버전 부분(3.05.2)을 파싱해 >= 3.05 로 판정하거나,
                    연결 후 SELECT VERSION() 결과(커뮤니티 기반 버전)로 확정한다.
                    **Aurora 3.x != MySQL 8.0.32+ 다** (3.01~3.04 는 8.0.23~8.0.28 기반)
4. 수집 활성 여부:
     UI 오버라이드가 있으면 그것
     없으면 태그 dbmon:enabled 값
     없으면 기본값(설정, 기본 true)
```

Aurora 클러스터 처리:
- 클러스터의 각 멤버 인스턴스를 개별 수집 대상으로 등록한다. `IsClusterWriter`로 라이터/리더를
  구분하고 `cluster_id`로 묶는다.
- **슬로우 쿼리는 라이터와 리더 모두에서 발생한다.** 리더를 빼면 리포팅 워크로드를 놓친다.
- 다이제스트 통계는 노드별로 독립이다(각 노드의 `performance_schema`가 따로 누적).
  클러스터 단위 집계는 멤버 합산으로 만든다.
- Aurora Serverless v2는 `ACUUtilization` 메트릭과 스케일 이벤트를 추가로 수집한다.

### 1.3 환경 분류

```
후보 태그 키 (대소문자 무시, 이 순서로 탐색):
  env, Environment, environment, ENV, stage, Stage, tier

값 정규화 매핑 (기본값, 설정으로 변경 가능):
  prd  ← prd, prod, production, live, real, p
  stg  ← stg, stage, staging, qa, uat, s
  dev  ← dev, develop, development, test, sandbox, local, d
  그 외 → unknown
```

- 매핑은 `dbmon-config / CFG / GLOBAL`의 `env_tag_mapping`에 저장하고 UI에서 편집한다.
- 어떤 후보 키도 없으면 `unknown`. 인스턴스 이름의 패턴 추론(`-prd-` 포함 등)은 **하지 않는다**
  — 추론이 틀리면 dev 알림이 prd 채널로 가는 사고가 난다. `unknown`으로 두고 사람이 지정하게 한다.
- 수동 지정(`INSTOVR`)은 태그 재수집에도 유지되며, 태그와 다르면 UI에 "태그와 불일치" 배지를
  표시한다(태그를 고칠 기회를 준다).

### 1.4 삭제 처리

```
탐색 결과에 없는 인스턴스:
  deleted_at = now 마킹 (즉시 삭제하지 않음)
  수집 태스크 정지
  30일 후 config 항목 삭제 (데이터 테이블의 레코드는 자체 TTL로 만료)
```

즉시 삭제하지 않는 이유: 과거 슬로우 쿼리 레코드가 인스턴스 메타(env, engine_version)를
참조한다. 다만 레코드에 이미 비정규화해 저장하므로([04 §2.3](04-data-model.md)) 조회는 깨지지
않는다. 30일 유지는 "어제 삭제한 인스턴스의 어제 데이터를 오늘 보는" 경우를 위한 것.

일시적 API 실패로 인스턴스가 사라진 것처럼 보이는 경우를 방어한다:
**2회 연속 탐색에서 없을 때만** `deleted_at`을 찍는다.

### 1.5 필요한 IAM 권한

```json
{
  "Effect": "Allow",
  "Action": [
    "rds:DescribeDBInstances",
    "rds:DescribeDBClusters",
    "rds:DescribeDBClusterParameters",
    "rds:DescribeDBParameters",
    "rds:DescribeDBParameterGroups",
    "rds:DescribeDBLogFiles",
    "rds:DescribePendingMaintenanceActions",
    "rds:DescribeDBEngineVersions",
    "rds:DescribeEvents",
    "rds:DescribeCertificates",
    "rds:ListTagsForResource"
  ],
  "Resource": "*"
}
```

`Resource: "*"`인 이유: **RDS Describe 액션의 리소스 레벨 지원은 액션별로 다르며**
(`DescribeDBInstances`·`DescribeDBClusters`는 `db`/`cluster` 리소스 타입을 갖지만
`DescribeDBEngineVersions`·`DescribeEvents`는 리소스가 없다), 우리 목적이 **계정 전체 목록
조회**라 특정 리소스로 좁히면 탐색 자체가 불가능하다. 읽기 전용이므로 위험도는 낮다.
NFR-S-09에 따라 근거를 남긴다.
(초기 문서는 "리소스 레벨 권한을 지원하지 않는다"고 단정했는데 이는 과장이었다.)

### 1.6 RDS 이벤트 수집 (FR-DSC-12)

`rds:DescribeEvents`로 인스턴스·클러스터·파라미터 그룹 이벤트를 15분마다 수집한다.

카테고리: `failover`, `failure`, `maintenance`, `configuration change`, `availability`,
`deletion`, `restoration`, `notification`, `read replica`.

**왜 중요한가** — "새벽 3시에 왜 갑자기 플랜이 바뀌고 지표가 튀었나"의 답이 대부분 여기 있다.
페일오버, 재부팅, 파라미터 그룹 적용, 스토리지 오토스케일링, 인스턴스 클래스 변경이 모두
이벤트로 나온다. 이걸 안 보면 사람이 매번 RDS 콘솔을 따로 확인해야 한다.

- `Event` 엔티티의 `kind = RDS_EVENT`로 저장하고 `event_category`·`message`를 담는다.
- **메트릭 차트와 슬로우 쿼리 타임라인에 이벤트 마커로 오버레이한다.**
  `PLAN_CHANGE` 이벤트와 시간이 겹치면 인과를 바로 볼 수 있다.
- 알림 소스로 제공한다(`rds_event` 소스 + 카테고리 필터).
- `DescribeEvents`는 최대 14일까지 조회 가능하다. 중복은
  `(source_identifier, event_time, message_hash)`로 제거한다.

### 1.7 대상 RDS 인증서 만료 감시 (FR-DSC-13)

앱 자신의 CA 번들 만료는 감시하는데([07 §3.3](07-credentials-bootstrap.md))
**대상 RDS의 인증서 만료는 감시하지 않았다.** RDS 루트 CA 만료는 전사 장애를 유발한 이력이
있는 항목이다.

```
DescribeDBInstances  → CertificateDetails { CAIdentifier, ValidTill }
DescribeCertificates → 계정에서 사용 가능한 CA 목록과 유효기간
```

- `ValidTill` 90일 미만 → 경고, 30일 미만 → critical 알림.
- `CAIdentifier`가 EOL 예정 CA면 경고.
- 자가진단 항목으로 노출하고 인스턴스 목록에 배지를 표시한다.

## 2. 메트릭 — 두 개의 서로 다른 소스

이 시스템에는 성격이 다른 두 메트릭 소스가 있다. **UI에서 반드시 시각적으로 구분해야 한다.**

| | 자체 수집 (`global_status` 델타) | CloudWatch |
|---|---|---|
| 해상도 | 5초 | 60초 (표준) |
| 지연 | < 1초 | 1~3분 |
| 보관 | 메모리 60분 + 시간 롤업 | 15개월 (자동 롤업) |
| 지표 | MySQL 내부 카운터 (QPS, 락 대기, 임시 테이블, 버퍼풀 히트…) | 호스트·스토리지·네트워크·엔진 (CPU, IOPS, 지연, 연결 수…) |
| 비용 | 0 | API 호출당 과금 |
| 대상 DB 부하 | 5초당 쿼리 1건 | 0 |
| 용도 | **"지금 무슨 일이 일어나는가"** | **"추세가 어떤가"** |

"실시간 CloudWatch 렌더링"은 물리적으로 불가능하다(CloudWatch 자체가 1~3분 지연).
진짜 실시간은 자체 수집이 담당하고, CloudWatch는 추세·용량·비교에 쓴다.

### 2.1 CloudWatch 메트릭 세트

**RDS MySQL / Aurora 공통**

| 메트릭 | 단위 | 통계 | 용도 |
|---|---|---|---|
| `CPUUtilization` | % | Average, Maximum | |
| `DatabaseConnections` | Count | Average, Maximum | 포화 판단 |
| `FreeableMemory` | Bytes | Minimum | |
| `ReadIOPS` / `WriteIOPS` | Count/s | Average | |
| `ReadLatency` / `WriteLatency` | Seconds | Average, p99 | |
| `ReadThroughput` / `WriteThroughput` | Bytes/s | Average | |
| `NetworkReceiveThroughput` / `NetworkTransmitThroughput` | Bytes/s | Average | |
| `DiskQueueDepth` | Count | Average | |
| `SwapUsage` | Bytes | Average | |

RDS for MySQL에는 `Deadlocks`·`EngineUptime`이 없다.
→ 데드락은 `SHOW ENGINE INNODB STATUS` 파싱(FR-OBS-03), 재시작은 자체 수집 `Uptime` 델타로
얻는다. **없는 메트릭을 요청하면 요청 수로 과금되면서 결과는 빈 값**이므로, 엔진별 메트릭
카탈로그(FR-MET-01)에서 반드시 분리해야 한다.

**RDS MySQL 전용**

| 메트릭 | 용도 |
|---|---|
| `FreeStorageSpace` | 스토리지 고갈 |
| `BinLogDiskUsage` | 바이너리 로그 누적 |
| `ReplicaLag` | 복제 지연 |
| `BurstBalance` | gp2 버스트 소진 |
| `EBSIOBalance%` / `EBSByteBalance%` | 인스턴스 레벨 EBS 버스트 |

**Aurora MySQL 전용**

| 메트릭 | 용도 |
|---|---|
| `AuroraReplicaLag` / `AuroraReplicaLagMaximum` | 리더 지연 |
| `BufferCacheHitRatio` | |
| `SelectLatency` / `DMLLatency` / `CommitLatency` / `DDLLatency` | 엔진 레벨 지연 |
| `SelectThroughput` / `DMLThroughput` / `CommitThroughput` | |
| `AuroraVolumeBytesLeftTotal` | 볼륨 한도 |
| `RollbackSegmentHistoryListLength` | 퍼지 지연 (장기 트랜잭션 신호) |
| `AuroraBinlogReplicaLag` | 바이너리 로그 복제 |
| `Deadlocks` | 데드락 발생률. **Aurora 전용.** 이미 초당 평균이므로 통계는 `Average`(Sum은 무의미) |
| `EngineUptime` | 재시작 감지. **Aurora 전용** |
| `ACUUtilization` / `ServerlessDatabaseCapacity` | Serverless v2 |
| `EngineCPUUtilization` | Aurora 엔진 CPU (호스트 CPU와 구분) |

`RollbackSegmentHistoryListLength`는 장기 트랜잭션·퍼지 지연을 잡는 데 매우 유용한데
자주 간과된다. 알림 기본 규칙에 포함한다.

### 2.2 `GetMetricData` 사용 규칙

```rust
// 단일 호출에 최대 500 MetricDataQuery
// 각 쿼리에 Id, MetricStat{Metric{Namespace, MetricName, Dimensions}, Period, Stat}
// ScanBy=TimestampDescending, MaxDatapoints 로 응답 크기 제한
```

- `GetMetricStatistics`는 쓰지 않는다(메트릭당 1호출).
- 기간에 따른 period 자동 선택:

| 조회 범위 | period | 데이터포인트 수 |
|---|---|---|
| ≤ 3시간 | 60s | ≤ 180 |
| ≤ 12시간 | 60s | ≤ 720 |
| ≤ 3일 | 300s | ≤ 864 |
| ≤ 2주 | 900s | ≤ 1344 |
| ≤ 3개월 | 3600s | ≤ 2160 |
| 그 이상 | 21600s | |

**CloudWatch의 최소 period 규칙 (정확히)**

| 조회 시작 시각 | 사용 가능한 최소 period |
|---|---|
| 3시간 이내 | 1초 (고해상도 메트릭만) |
| 3시간 ~ 15일 전 | **60초** (및 60의 배수) |
| 15일 ~ 63일 전 | **300초 이상** |
| 63일 초과 | **3600초 이상** |

보관 기간: 1분 데이터 15일 / 5분 데이터 63일 / 1시간 데이터 455일.

초기 설계는 "63일 초과는 5분 이상, 15개월은 1시간"이라고 썼는데 **15~63일 구간의 300초
제약이 빠져 있었다.** 그대로 구현하면 30일 전 구간에 60초를 요청해 빈 결과를 받는다.
→ period 자동 조정은 **조회 시작 시각**을 기준으로 판정하고, 조정했으면 응답에
`period_adjusted: true`와 조정 이유를 담아 UI에 표시한다.

### 2.3 비용 통제 (가장 중요한 설계 제약)

순진하게 구현하면 이 항목이 전체 비용을 지배한다.

```
500 인스턴스 × 15 메트릭 = 7,500 메트릭/호출
60초마다 전체 폴링 → 1,440 호출/일 → 10.8M 메트릭요청/일
GetMetricData 과금 $0.01 / 1,000 메트릭 → 일 $108 → 월 $3,240
```

**설계**

| 계층 | 대상 | 주기 | 월 메트릭 요청 |
|---|---|---|---|
| 플릿 개요 | **CloudWatch 미사용.** 자체 수집 지표로 렌더 | 5초 | 0 |
| 플릿 핵심 지표 | 엔진별 3개 메트릭 × 500대 (아래 표) | 15분 | 500×3×2,880 = 4.32M → **$43/월** |
| 인스턴스 상세 | 전체 메트릭 세트, **화면을 보고 있을 때만** | 60초, 60초 캐시 | 실사용 비례, 월 수십 달러 |
| 기간 조회 | 사용자가 기간 선택 시 1회 | 온디맨드, 결과 캐시 | 소액 |

**엔진별 플릿 메트릭 세트** — 하나의 세트로 통일할 수 없다.

| 엔진 | 플릿 메트릭 | 이유 |
|---|---|---|
| RDS MySQL | `CPUUtilization`, `FreeableMemory`, `FreeStorageSpace` | |
| Aurora MySQL (인스턴스) | `CPUUtilization`, `FreeableMemory` | **`FreeStorageSpace`가 Aurora에 없다** |
| Aurora MySQL (클러스터별 1개) | `AuroraVolumeBytesLeftTotal` | 볼륨 한도는 클러스터 레벨 |

**자체 수집으로 대체한 것**
- 연결 수 → `Threads_connected` (global_status)
- 복제 지연 (RDS) → `SHOW REPLICA STATUS`
- 복제 지연 (**Aurora**) → **`SHOW REPLICA STATUS`는 Aurora 리더에서 빈 결과다**
  (스토리지 레벨 복제이므로 바이너리 로그 복제가 아니다).
  → `information_schema.replica_host_status`의 `REPLICA_LAG_IN_MILLISECONDS`를 쓴다(비용 0).
  이것도 안 되면 `AuroraReplicaLag`를 4번째 메트릭으로 추가(+약 $14/월).

최종: **플릿 폴링은 엔진별 3메트릭 × 15분 = $43/월**, 나머지는 온디맨드.

**월 예산 상한**(FR-MET-09): 사용량을 일별로 집계해 예산의 80% 도달 시 경고, 100% 도달 시
온디맨드 조회를 캐시 강제 모드로 전환한다. 사용량을 UI에 항상 노출한다.

**캐시** — `(instance_id, metric_name, period, aligned_time_window)` 키로 메모리 캐시.
period 경계에 정렬하므로 여러 사용자가 같은 화면을 봐도 1회만 호출한다.

### 2.4 Metric Streams는 쓰지 않는다

대안으로 CloudWatch Metric Streams → Firehose → S3를 검토했다.
$0.003/1,000 메트릭 업데이트로 단가는 싸지만, **모든 메트릭이 계속 흐르므로**
500대 × 15메트릭 × 1분 = 월 324M 업데이트 = **$972/월**. 온디맨드 조회보다 비싸다.
필요한 것만 필요할 때 가져오는 게 이 워크로드에 맞다.

### 2.5 슬로우 쿼리 오버레이 (FR-MET-07)

메트릭 차트 위에 슬로우 쿼리 발생 시점을 마커로 얹는다.
- 데이터: 해당 인스턴스·기간의 슬로우 쿼리를 시간 버킷으로 집계(차트 period와 동일 정렬)
- 렌더: uPlot 플러그인으로 x축 위에 밀도 막대 + hover 시 상위 다이제스트 툴팁
- "CPU가 튄 시점에 어떤 쿼리가 돌았나"에 클릭 없이 답하는 것이 목적

### 2.6 Enhanced Monitoring (FR-MET-08, P2)

OS 레벨 지표(프로세스별 CPU, 디스크 I/O 상세)는 CloudWatch **메트릭이 아니라 로그**
(`RDSOSMetrics` 로그 그룹)로 들어온다. 최소 1초 granularity.

- 활성 인스턴스에 대해서만 `FilterLogEvents`로 최근 N개 이벤트를 읽어 파싱.
- 상시 수집하지 않는다(로그 조회 비용 + 볼륨). 상세 화면에서 "OS 지표 보기"를 눌렀을 때만.

## 3. 필요한 CloudWatch IAM 권한

```json
{
  "Effect": "Allow",
  "Action": [
    "cloudwatch:GetMetricData",
    "cloudwatch:ListMetrics",
    "logs:DescribeLogGroups",
    "logs:DescribeLogStreams",
    "logs:FilterLogEvents",
    "logs:GetLogEvents",
    "logs:StartQuery",
    "logs:GetQueryResults"
  ],
  "Resource": "*"
}
```

`logs:FilterLogEvents`의 리소스는 `arn:aws:logs:*:<account>:log-group:/aws/rds/*`로 제한할 수
있다. `cloudwatch:GetMetricData`는 리소스 레벨 권한을 지원하지 않는다.

## 4. 멀티 리전 처리

```
설정된 리전 목록마다 클라이언트 인스턴스를 캐시한다.
  rds_clients:   HashMap<Region, rds::Client>
  cw_clients:    HashMap<Region, cloudwatch::Client>
  logs_clients:  HashMap<Region, logs::Client>

탐색: 리전 병렬 (동시 8개). 한 리전 실패가 다른 리전을 막지 않는다.
메트릭: 인스턴스의 리전 클라이언트로 호출. GetMetricData 배치는 리전별로 분리.
```

리전 목록은 설정(`CFG/GLOBAL/regions`)에서 관리하고, `ec2:DescribeRegions`로 후보를 제시한다.
활성 리전 자동 전체 스캔은 하지 않는다(불필요한 API 호출과 권한 확대).

**크로스 리전 MySQL 연결** — 앱은 1개 리전에 배포되고 대상 RDS는 여러 리전에 있다.
연결 지연이 왕복 수십~수백 ms이므로:
- 1초 폴링에서 왕복 200ms는 허용 범위(예산의 20%)
- 왕복 500ms를 넘는 리전은 폴링 주기를 2초로 자동 상향하고 UI에 표시
- 지연이 큰 리전의 인스턴스가 많으면 해당 리전에 collector를 별도 배치
  → [OPEN-Q-02](OPEN-QUESTIONS.md)

VPC 피어링 / Transit Gateway / VPC 엔드포인트 구성은 [14-infrastructure.md](14-infrastructure.md).

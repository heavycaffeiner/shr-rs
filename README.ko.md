# shr-rs

*This document in [English](README.md).*

**SHR, Sliced Hybrid RAID: 크기가 서로 다른 디스크를 하나의 저장 공간으로,
리눅스 기본 도구만으로.** `mdadm` + `LVM` + `Btrfs`를 씁니다. 별도 커널 모듈도,
독자 포맷도, 전용 하드웨어도 없습니다. 내일 shr-rs가 사라져도 여러분의 어레이는
이미 시스템에 깔려 있는 도구들로 그대로 조립됩니다.

**한 문장으로:** 크기가 제각각인 하드디스크들이 있고, 이것들이 디스크 한 개쯤
고장 나도 버티는 하나의 큰 드라이브처럼 동작하기를 원하며, 큰 디스크의 남는
공간을 버리고 싶지도 않다면. 그걸 해주는 도구입니다.

RAID가 처음이신가요? 아래 상자를 먼저 펼쳐 보세요. 이후 내용은 여기 나온 몇
단어를 안다고 가정합니다.

<details>
<summary><b>이 문서에서 쓰는 용어</b></summary>

- **RAID**는 데이터를 여러 디스크에 동시에 두어, 디스크 하나가 죽어도 데이터가
  사라지지 않게 합니다. 백업이 아닙니다. 디스크 고장을 막아줄 뿐, 파일을 잘못
  지운 것은 막아주지 못합니다.
- **패리티(parity)** 는 여분의 사본, 또는 그 역할을 대신하는 계산값입니다.
  4 TB 디스크 네 개가 16 TB가 아니라 12 TB를 담는 이유가 이것입니다. 디스크
  하나 분량을 써서 어느 하나가 죽어도 되게 만듭니다.
- **RAID5**는 디스크 한 개 고장을 버티고 용량 한 개를 씁니다. **RAID6**는 두
  개를 버티고 두 개를 씁니다. **RAID1**은 디스크 두 개를 그대로 복제하는
  방식으로, 용량은 절반이지만 둘 중 아무거나 죽어도 됩니다.
- **밴드(band)** 는 이 프로젝트가 쓰는 표현입니다. 크기가 다른 디스크들을 가로로
  잘라 슬라이스를 만들고, 각 슬라이스가 저마다 작은 RAID가 됩니다. 아래 그림을
  보세요.
- **고립(stranded) 공간**은 실재하지만 보호할 수 없는 용량입니다. 그 높이까지
  닿는 디스크가 하나뿐이기 때문입니다. shr-rs는 이 공간을 쓸 수 있는 용량으로
  세지 않고 따로 보여줍니다.
- **mdadm, LVM, Btrfs**는 이 도구가 부리는 세 가지 리눅스 표준 도구입니다.
  `mdadm`이 RAID를 만들고, `LVM`이 그것들을 하나의 볼륨으로 붙이고, `Btrfs`가
  실제로 파일을 담는 파일시스템입니다. 셋 다 배포판에 기본으로 들어 있으며, 이
  프로젝트는 그것들에게 지시를 내릴 뿐 그 이상은 하지 않습니다.
- **스크럽(scrub)** 은 디스크에 담긴 내용을 전부 한 번 읽어, 정작 데이터가
  필요할 때가 되기 전에 손상을 찾아내는 정기 점검입니다. 한 달에 한 번쯤
  돌리면 되고, 스케줄러가 대신 해줍니다.

</details>

## 왜 필요한가

4 TB, 4 TB, 6 TB, 8 TB 디스크를 하나의 RAID5로 묶으면 mdadm은 각 디스크에서
4 TB씩만 씁니다. 6 테라바이트, 지불한 용량의 4분의 1이 넘는 공간이 그냥
사라집니다.

shr-rs는 용량이 바뀌는 모든 경계에서 디스크를 자르고, 각 밴드마다 *별도의*
어레이를 만듭니다. 그 밴드에 참여하는 디스크 수가 허용하는 가장 좋은 이중화
수준으로요. LVM이 그 어레이들을 하나의 볼륨으로 이어 붙이고, 그 위에 Btrfs가
올라갑니다.

```console
$ shr-rs plan create --mode shr --sizes 4TB,4TB,6TB,8TB

Planned layout (mode: shr, DRY RUN)

  BAND   LEVEL         SLICE MEMBERS      USABLE
  band0  raid5        4.0 TB       4     12.0 TB
  band1  raid1        2.0 TB       2      2.0 TB

  Usable 14.0 TB   Parity 6.0 TB   Stranded 2.0 TB   Raw 22.0 TB
  [#########################+++++++++++....]  (9% wasted)

  ! disk3: 2001454759936 B stranded (no redundancy)
```

이 문서에서 가장 빽빽한 출력이니 한 줄씩 읽어보겠습니다.

- `band0  raid5  4.0 TB  4  12.0 TB`: 네 디스크 모두가 각자의 첫 4 TB를
  내놓고, 그 네 슬라이스가 RAID5를 이루며, 패리티를 뺀 뒤 이 밴드는 12 TB를
  담습니다.
- `band1  raid1  2.0 TB  2  2.0 TB`: 그보다 위까지 닿는 디스크는 둘(6 TB와
  8 TB)뿐이라, 이들의 다음 2 TB가 짝을 지어 2 TB짜리 미러가 됩니다.
- `Stranded 2.0 TB`: 8 TB 디스크의 맨 윗부분입니다. 그 높이까지 닿는 두 번째
  디스크가 없어 아무것도 보호해 줄 수 없고, shr-rs는 이를 용량으로 세기를
  거부합니다.
- 막대는 같은 이야기를 그림으로 보여줍니다. `#`은 데이터, `+`는 패리티, `.`은
  고립 공간입니다.

12 TB 대신 14 TB를 쓰면서도 여전히 디스크 한 개 고장을 견딥니다. 게다가 보호할
수 없는 2 TB가 어디인지, 왜 그런지를 조용히 용량에 섞어 넣지 않고 분명히
말해줍니다. 이 자르기가 이름 그대로입니다. **SHR**은 *Sliced Hybrid RAID*이고,
하이브리드인 이유는 하나의 저장 공간이 동시에 여러 RAID 수준이기 때문입니다.
**SHR-2**(`--mode shr2`)는 같은 아이디어를 디스크 두 개 고장 허용으로 올린
것입니다.

같은 두 밴드를 웹 대시보드에서 본 모습입니다. 그룹 하나, 서로 다른 RAID 수준의
mdadm 어레이 둘, 각각의 슬라이스 크기와 멤버와 재구성 상태가 보입니다.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/group-bands-dark.png">
  <img alt="SHR 그룹 패널: 그룹 shr1에 band0이 4.0 TB 슬라이스 세 개의 RAID5로, band1이 3.9 TB 슬라이스 두 개의 RAID1로 구성되어 있다. 각 밴드마다 mdadm 디바이스와 UUID, 결함 표시가 붙은 멤버 디바이스, 가용 용량 대비 전체 용량, 동기화와 스크럽 상태가 나열된다." src="docs/images/group-bands-light.png">
</picture>

## 동작 방식

```
디스크 → GPT 파티션 → mdadm 밴드 (md0, md1, …) → LVM VG (linear) → LV → Btrfs (zstd)
```

- **밴드.** 용량이 달라지는 지점마다 참여 디스크당 파티션 하나와 mdadm 어레이
  하나가 생깁니다. 한 밴드에 디스크가 넷이면 RAID5(SHR-2에서는 RAID6), 둘이면
  RAID1, 하나면 그 공간은 고립되고 그렇게 보고됩니다.
- **LVM.** 어레이들은 하나의 볼륨 그룹으로 선형 연결됩니다. 그래서 나중에 밴드가
  늘어도 두 번째 파일시스템이 생기는 대신 쓰던 파일시스템이 늘어납니다.
- **Btrfs.** 투명한 zstd 압축, 서브볼륨(`@`, `@snapshots`), 보존 기간이 있는
  스냅샷, 그리고 mdadm과 별개로 자체 스크럽을 제공합니다.
- **확장은 온라인으로.** 디스크를 추가하면 밴드를 다시 계획하고, 어레이를 제자리
  확장하고, VG와 LV와 파일시스템을 늘리고, 실사용 I/O에 맞춰 재구성 속도를
  조절합니다. 모양을 바꾸는 동안에도 어레이는 계속 쓸 수 있습니다.

## 설치

패키지는 둘입니다. `shr-rs`가 엔진이고 CLI와 TUI를 포함하며 그 자체로 쓸 수
있습니다. `cockpit-shr-rs`는 선택 사항인 웹 대시보드로 엔진에 의존합니다.

`v*` 태그마다
[GitHub Releases](https://github.com/heavycaffeiner/shr-rs/releases)에
게시됩니다.

| 대상 | 엔진 | 대시보드 |
|---|---|---|
| Rocky / RHEL / CentOS Stream **9** | `shr-rs-*.el9.x86_64.rpm` | `cockpit-shr-rs-*.el9.noarch.rpm` |
| Rocky / RHEL / CentOS Stream **10** | `shr-rs-*.el10.x86_64.rpm` | `cockpit-shr-rs-*.el10.noarch.rpm` |
| Debian / Ubuntu | `shr-rs_*_amd64.deb` | `cockpit-shr-rs_*_all.deb` |
| Arch | `shr-rs-*-x86_64.pkg.tar.zst` | `cockpit-shr-rs-*-any.pkg.tar.zst` |
| 그 밖 | `shr-rs-*-x86_64.tar.gz` | `cockpit-shr-rs-*.tar.xz` |

```bash
gh release download v0.4.1 -R heavycaffeiner/shr-rs
sha256sum -c SHA256SUMS

sudo dnf install ./shr-rs-*.rpm ./cockpit-shr-rs-*.rpm              # EL9 / EL10
sudo apt install ./shr-rs_*.deb ./cockpit-shr-rs_*.deb              # Debian / Ubuntu
sudo pacman -U ./shr-rs-*.pkg.tar.zst ./cockpit-shr-rs-*.pkg.tar.zst  # Arch

sudo systemctl restart cockpit.socket   # 대시보드를 설치했을 때만
```

엔진은 어느 패키지에서나 동일한 정적 링크 musl 바이너리이고 대시보드도 동일한
사전 빌드 번들입니다. 패키지끼리는 메타데이터만 다르므로 어느 쪽도 이류 포팅이
아닙니다. `btrfs-progs`와 `smartmontools`는 권장이지 필수가 아닙니다. mdadm과
LVM 관리는 둘 다 없어도 동작합니다.

`shr-rs.service`는 **비활성 상태로 배포되며, 켤 필요가 없습니다.** 하는 일이
10초마다 상태를 다시 출력하는 것뿐입니다. 실제 정기 작업을 하는 타이머들(오류
점검, 재구성 속도 조절, 상태 점검, 스냅샷)은 `shr-rs schedule install`이
만듭니다.

## 빠른 시작

**여기서 하는 일은 지정한 디스크를 지웁니다.** 다만 4단계의 `create` 전까지는
아무것도 디스크에 쓰지 않고, 그 `create`도 실행 전에 멈춰서 그룹 이름을 직접
입력하라고 요구합니다. 1단계부터 3단계까지는 아무것도 바꾸지 않으니 마음 놓고
돌려보세요.

먼저, 어떤 디스크가 여러분 것인가요? 아래의 `sdb,sdc,sdd,sde`는 예시이지
기본값이 아닙니다. 직접 확인하세요.

```bash
lsblk -o NAME,SIZE,MODEL,MOUNTPOINTS
```

마운트 지점이 없고 남겨둘 것이 없는 디스크를 고르세요. 시스템이 부팅하는
디스크는 CLI와 대시보드 양쪽에서 자동으로 거부되므로 실수로 고를 수 없습니다.

```bash
# 1. 이 디스크들을 써도 안전한가? 아무것도 바꾸지 않음.
sudo shr-rs preflight --disks sdb,sdc,sdd,sde

# 2. 어떤 배치가 나오는가? 아무것도 바꾸지 않음.
sudo shr-rs plan create --mode shr --disks sdb,sdc,sdd,sde

# 3. 실행될 모든 명령을 출력만 하고, 하나도 실행하지 않음.
sudo shr-rs create --mode shr --disks sdb,sdc,sdd,sde --name tank --dry-run

# 4. 실제로 실행.
sudo shr-rs create --mode shr --disks sdb,sdc,sdd,sde \
     --name tank --mount /mnt/tank --vg-name tank_vg

# 5. 현재 상태.
sudo shr-rs status --detail
```

이후 파일은 `/mnt/tank`에 놓입니다. 어레이가 백그라운드에서 스스로를 다 만드는
동안에도 계속 사용할 수 있고, 그 진행 상황은 `status`에 나옵니다. 아래의
`schedule install`로 정기 관리를 한 번 설정해 두면 그 뒤로는 신경 쓰지 않아도
됩니다.

나중에 디스크를 추가할 때도 같은 모양이며, `--dry-run`을 먼저 씁니다.

```bash
sudo shr-rs expand --name tank --add sdf --dry-run
sudo shr-rs expand --name tank --add sdf
```

정기 관리:

```bash
sudo shr-rs schedule install --name tank   # 오류 점검과 상태 점검 타이머
sudo shr-rs fs scrub start --name tank     # mdadm check와 Btrfs scrub
sudo shr-rs fs scrub start --priority max  # 디스크가 허용하는 최대 속도로
sudo shr-rs fs scrub speed --priority max  # 이미 진행 중인 검사 속도를 바꾸기
sudo shr-rs disk list                      # SMART 상태가 포함된 목록
sudo shr-rs fs df                          # 실제 Btrfs 사용량과 여유 공간
sudo shr-rs snapshot create --name tank
```

나머지는 `shr-rs --help`에 있습니다. `groups`, `reconcile`, `destroy`,
`fs recompress`, `disk replace` 등입니다.

## 인터페이스 셋, 엔진 하나

모든 프런트엔드는 동일한 내부 명령 API 위의 얇은 클라이언트입니다. 그래서 어느
하나가 다른 쪽이 볼 수 없는 일을 할 수는 없습니다.

- **CLI.** 서브커맨드가 스크립트 가능한 경로를 실행합니다. `--json`을 붙이면
  스키마 버전이 붙은 기계 판독용 출력이 나옵니다.
- **TUI.** 터미널에서 인자 없이 `shr-rs`를 실행하면 대화형 대시보드가 뜹니다.
  디스크, 어레이, 그룹, 밴드, 파일시스템, 로그가 실시간으로 갱신되고, 디스크
  추가 마법사가 안내해 줍니다.
- **Cockpit.** 웹 대시보드는 같은 `status --json` 페이로드를 렌더링하고, 그룹
  생성 마법사와 운영 패널(스크럽, 확장, 교체, 재압축, 스냅샷, 스케줄)을 더합니다.
  밝기 설정은 스스로 정하지 않고 Cockpit 셸의 설정을 따라가며, 화면이 좁아지면
  휴대폰 폭까지 재배치됩니다.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/dashboard-dark.png">
  <img alt="SHR-RS 대시보드: 백그라운드 작업, 전체 상태, 구성 드라이브, 감지된 원시 용량, RAID 멤버를 보여주는 요약 타일 행과, 가용 스토리지, 사용 중 공간, 여유 공간, 보호 레벨을 보여주는 두 번째 행, 그 아래 저장 공간을 사용 중, 여유, 패리티, 시스템 디스크 몫으로 나눈 할당 현황 카드." src="docs/images/dashboard-light.png">
</picture>

<sub>이 스크린샷은 보시는 분의 밝기 설정을 따라갑니다. 대시보드가 Cockpit의
설정을 따라가는 것과 같은 방식입니다.</sub>

휴대폰 폭에서는 표가 항목 이름이 붙은 행으로 쌓이고, 설명 목록이 세로로 바뀌며,
버튼 줄이 카드 밖으로 흘러넘치는 대신 접힙니다.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/mobile-dark.png">
  <img alt="같은 그룹 패널을 390픽셀 폭에서 본 모습: 밴드 표가 항목 이름이 붙은 행으로 쌓여 한 줄에 한 항목씩 표시되고, 파일시스템 UUID와 멤버 디스크 이름이 카드 안에서 줄바꿈된다." src="docs/images/mobile-light.png" width="320">
</picture>

<sub>스크린샷은 예시 데이터를 씁니다. 배치는 실제 배포되는 빌드 그대로입니다.</sub>

## 안전 장치

스토리지 도구는 잘못되면 크게 잘못되므로, 여기 기본값들은 급한 사람에게
일부러 불친절합니다.

- **미리보기 없이는 파괴적인 일이 일어나지 않습니다.** `create`, `expand`,
  `destroy` 모두 `--dry-run`을 받고, Cockpit 쪽 대응 기능은 미리보기를 보여주기
  전까지 실행을 거부합니다.
- **되돌릴 수 없는 작업은 클릭이 아니라 그룹 이름을 직접 입력하기를 요구합니다.**
  `--yes`는 스크립트를 위한 것이고, 이를 건너뛰는 유일한 방법입니다.
- **롤백이 저널에 기록됩니다.** 도중에 실패한 `create`나 `expand`는 반쯤 만들다
  만 어레이를 남기는 대신 이미 밟은 단계를 되감습니다.
- **중단된 확장은 이어서 진행됩니다.** 체크포인트가 크래시나 정전을 견디고,
  `reconcile`이 재구성이 끝나기를 기다려야 했던 부분을 마무리합니다.
- **사용 중인 디스크는 거부됩니다.** 프리플라이트가 후보를 정확히 대조하고,
  시스템 디스크는 CLI와 대시보드 선택기 양쪽에서 거부됩니다.
- **대시보드는 진짜 권한을 요구합니다.** 모든 쓰기는 Cockpit의
  `superuser: "require"`를 거치며, `"try"`는 쓰지 않습니다.

## 제약

- **x86_64 리눅스 전용.** 패키지는 한 아키텍처로만 빌드됩니다.
- **Btrfs는 그것이 들어 있는 커널을 필요로 합니다.** Rocky와 RHEL의 기본 커널은
  EL9에도 EL10에도 Btrfs 모듈이 없습니다. `btrfs-progs`는 EPEL에 있지만 대화할
  상대가 없고, ELRepo도 `kmod-btrfs`를 내놓지 않습니다. 해결책은 `btrfs.ko`를
  포함한 ELRepo 커널입니다. EL10에서는 `kernel-ml`(Rocky 10에서 전체 스택을
  검증할 때 7.1.5를 썼습니다), EL9에서는 `kernel-lt`입니다. 설치하고 그 커널로
  부팅한 뒤 `modprobe btrfs`를 실행하세요. Debian과 Arch는 기본 커널에 Btrfs가
  들어 있습니다. 이 도구가 하는 일의 대부분인 mdadm과 LVM은 어디서나 그대로
  동작합니다.
- **아직 어린 프로젝트입니다.** 엔진은 실제 디스크와 에뮬레이트된 디스크에서
  실제 mdadm을 상대로 생성, 확장, 성능 저하, 재구성, 스크럽, 교체, 재부팅
  생존까지 검증했지만, 모든 배치의 모든 경로를 다 해본 것은 아닙니다. 잃으면 안
  되는 데이터를 맡기기 전에 `--dry-run` 출력을 읽어보세요.

## 소스에서 빌드하기

엔진은 Rust 툴체인이 있는 어느 호스트에서든 정적 musl 바이너리로
크로스컴파일됩니다. 대시보드는 node 22.18 이상이 필요한 평범한 npm 빌드입니다.

```bash
cargo build --release --target x86_64-unknown-linux-musl --workspace
(cd cockpit && npm ci --ignore-scripts && npm run build)
```

```bash
sudo install -m755 target/x86_64-unknown-linux-musl/release/shr-rs /usr/bin/shr-rs
sudo mkdir -p /usr/share/cockpit/shr-rs && sudo cp -r cockpit/dist/* $_
sudo systemctl restart cockpit.socket
```

`/usr/local/bin`이 아니라 `/usr/bin`입니다. 대시보드는 cockpit-bridge 세션
안에서 `PATH`를 뒤져 바이너리를 찾기 때문입니다.

양쪽 모두 서드파티 코드를 함께 배포합니다. 엔진은 약 120개 크레이트와 musl
libc를 정적으로 링크하고, 대시보드는 PatternFly와 React와 그 웹폰트를
번들합니다. 그래서 양쪽 다 각각의 라이선스를 나열한 고지 파일을 생성합니다.
대시보드는 `npm run build`가 `cockpit/dist/THIRD-PARTY-NOTICES.txt`를 알아서
쓰고, 엔진 쪽은 별도 단계이며 릴리스 워크플로가 패키징 전에 실행합니다.

```bash
cargo install cargo-about --locked --features cli
cargo about generate --config about.toml about.hbs -o THIRD-PARTY-NOTICES.txt
cat packaging/notices/musl-HEADER.txt \
    packaging/notices/musl-COPYRIGHT.txt >> THIRD-PARTY-NOTICES.txt
```

`about.toml`은 허용 라이선스 집합도 고정합니다. 검토되지 않은 조건의 의존성이
들어오면 고지 없이 배포되는 대신 그 명령이 실패합니다.

테스트는 `cargo test --workspace`, 그리고 `cockpit/`에서 `npm test`,
`npm run typecheck`, `npm run eslint`입니다.
[`.github/workflows/release.yml`](.github/workflows/release.yml)이 패키징의
기준 레시피입니다. 배포판 계열마다 컨테이너 하나면 손으로도 재현할 수 있고,

```bash
gh workflow run release.yml -f version=0.0.0
```

는 아무것도 게시하지 않은 채 전체 매트릭스를 빌드하고 설치까지 확인합니다.

## 라이선스

Rust 워크스페이스는 `MIT OR Apache-2.0` 중 선택입니다.
[LICENSE-MIT](LICENSE-MIT)와 [LICENSE-APACHE](LICENSE-APACHE)를 보세요.
`cockpit/` 아래의 Cockpit 대시보드는 Cockpit starter-kit에서 파생되었으므로
`LGPL-2.1-or-later`로 유지됩니다([cockpit/LICENSE](cockpit/LICENSE)).

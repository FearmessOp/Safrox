# ADR 055 — Klavye–konsol topolojisi ve yalıtım

- **Durum:** Kabul edildi
- **Tarih:** M15
- **Bağlam:** ADR 018 (CSpace), ADR 037 (badge), ADR 043 (bileşen
  yükleme), ADR 046 (sürücü bileşeni deseni), ADR 053 (klavye),
  ADR 054 (konsol)

## Bağlam

M13 bir sürücü ve bir istemci kurmuştu: istemci Endpoint'e yazıyor,
sürücü donanıma. M15'te iki bileşen de kendi donanımına sahip —
biri klavyeyi, diğeri ekranı sürüyor — ve aralarında veri akıyor.

Sınanacak iddia: **bir bileşenin yetkisi, işini yapmaya yetecek
kadar ve o kadar.** Klavye ekranı göremiyor, konsol klavye portuna
erişemiyor, ve ikisi de birbirinin belleğine yazamıyor.

## Karar: tek yönlü Endpoint, klavyeden konsola

```
  klavye                                konsol
  ┌──────────────┐                      ┌──────────────┐
  │ IOPort 0x60  │                      │ 6 x device   │
  │ IOPort 0x64  │      Endpoint        │   Untyped    │
  │ IRQHandler 1 │ ───── Send ────────► │ (framebuffer)│
  │ Notification │                      │ karakter     │
  │              │                      │   ızgarası   │
  │              │                      │ Endpoint     │
  └──────────────┘                      └──────────────┘
```

Her bileşenin tuttuğu capability'ler, ve tutmadıkları:

| | klavye | konsol |
|---|---|---|
| IOPort 0x60, 0x64 | **var** | yok |
| IRQHandler 1 + Notification | **var** | yok |
| Framebuffer device Untyped | yok | **var** |
| Endpoint (ortak) | gönderen | alan |
| Untyped (sıradan) | yok | yok |
| IOPortControl / IRQControl | yok | yok |
| Diğerinin TCB/CNode/VSpace'i | yok | yok |

### Neden tek yönlü

Konsolun klavyeye söyleyeceği bir şey yok. İki yönlü bir kanal
(örneğin "LED'leri yak") PS/2 ilklendirmesi gerektirirdi, ki kapsam
dışı (ADR 053).

Tek yön, yalıtım iddiasını da keskinleştiriyor: konsol klavyeye
*hiçbir* capability tutmuyor, yalnızca ortak Endpoint'in alıcı
tarafında bekliyor.

### Badge: klavyenin kimliği

Endpoint konsola **unbadged** kopyalanıyor (alıcı), klavyeye
**badge'li** mint ediliyor (gönderen) — M13'ün deseni. Badge
`0xKBD0`.

Tek gönderen varken badge gereksiz görünüyor, ve değil: konsolun
`Recv`'i badge'i okuyup doğruluyor. Beklenmeyen bir badge'li mesaj
sayılıyor ve raporlanıyor. İkinci bir girdi kaynağı (fare, seri
konsol) geldiğinde ayrım zaten orada olacak.

## Karar: iki bileşen de sıradan Untyped tutmuyor

İlk tasarım konsola bir Untyped veriyordu — 3,9 MiB'lık piksel
gölge tamponunu kendi retype etsin diye. ADR 054'ün düzeltmesiyle o
tampon yok: konsol 8 KiB'lık bir **karakter ızgarası** tutuyor ve o
da yığınında duruyor.

Dolayısıyla ikisi de yalnızca kendilerine verilenle çalışıyor,
çalışma zamanında hiçbir nesne yaratmıyor. Konsolun tek ayrıcalığı
framebuffer'ın **device** Untyped'ları, ki onlar yalnızca Frame
olabilir (ADR 017) — bir TCB ya da CNode değil.

Bu, kaybedilmiş değil kazanılmış bir kısıt: "hiçbir bileşen sıradan
Untyped tutmuyor" tarif etmesi daha kolay ve ihlal edilmesi daha
görünür bir özellik.

### Düzeltme: halka tampon `.bss`'te olamaz

Bu ADR'nin ilk hâli "halka tampon kendi `.bss`'inde duruyor"
diyordu. **Yanlış, ve derlenmezdi.**

`driver/link.ld` iki iddia taşıyor:

```
ASSERT(SIZEOF(.bss) == 0, ...)
ASSERT(SIZEOF(.data) == 0, ...)
```

Gerekçesi orada yazılı: bileşenin imajı **read-execute** eşleniyor,
yani bir `static mut` ilk yazmada fault verirdi; ayrıca root task
flat bir imaj kopyalıyor ve hiçbir şeyi sıfırlamıyor, yani `.bss`
diye bir bölge zaten ayrılmıyor.

Tampon **yığında**, `serve()`'ün yerel değişkeni olarak duruyor.
Seri sürücü de durumunu (kaç bayt yazdı, ne okudu) böyle tutuyor ve
link betiği bunu açıkça söylüyor: *"The state lives on the stack, in
the main loop's locals."*

32 × 2 bayt = 64 bayt, tek sayfalık yığında sorun değil. M14'te root
task'ın 16 KiB'lık bir tamponu 4 KiB yığına koymaya çalışıp
`0x4fffb0`'da fault alması, bu sınırın gerçek olduğunun kanıtı.

Hatanın kaydedilme sebebi: ADR, var olmayan bir mekanizmayı tarif
ediyordu (ADR 023, K10). Kod yazılırken değil, link betiği okunurken
yakalandı — yani yakalanması bir tesadüftü ve ADR yazılırken
`driver/link.ld`'nin okunmamış olması asıl eksiklik.

## Konsolun CSpace'i: ekranınkiyle aynı, radix 11

`COMPONENT_CNODE_RADIX = 6` (64 slot) sıradan bileşenler için.
Ekran bileşeni 11 kullanıyor (2048 slot), çünkü framebuffer'ın her
sayfası bir Frame capability'si ve bir slot istiyor — M14'te 6 ile
denenmiş ve 48. sayfada `CPtrOutOfRange` ile durmuştu.

Konsol aynısına ihtiyaç duyuyor, fazlasına değil:

| | slot |
|---|---|
| Framebuffer Frame'leri | 1000 |
| PageTable'lar | ~2 |
| Sabit capability'ler (VSpace, Endpoint, device Untyped) | ~10 |

~1012, radix 11'in 2048'i içinde rahatça.

**İlk tasarımda radix 12 (4096 slot) seçilmişti**, gölge tamponun
1000 Frame'i daha sayıldığı için — toplam ~2024, radix 11'in
sınırına 24 slot kalıyordu. Izgara kararı o bin Frame'i ortadan
kaldırdı, ve onunla birlikte 128 KiB'lık CNode'u da: radix 11 bir
CNode nesnesi olarak 2048 × 32 = 64 KiB.

ADR 022'nin istediği bu: maliyet görünür, bir bileşene ait, ve
gerekmediğinde ödenmiyor.

Yine de M14'ün dersi duruyor: CSpace'in tükenmesi belleğin
tükenmesinden önce geliyor ve hata mesajı (`CPtrOutOfRange`) sebebi
söylemiyor.

## Konsolun yığını: dört sayfa

Her bileşen tek sayfalık yığın alıyor. Konsolun karakter ızgarası
8000 bayt ve `serve()`'ün yereli — yani tek sayfaya sığmıyor.

Root task ona **dört sayfa** eşliyor. Alternatif, ızgarayı
read-write eşlenmiş bir Frame'e koymaktı; işe yarardı ve bir şey
kazandırmıyor: ızgara özel, tek thread'li, ve bir yığının
tutabileceği kadar büyük.

M14'te root task'ın kendi 4 KiB yığınına 16 KiB'lık bir tampon
koymaya çalışıp `0x4fffb0`'da fault alması, bu sınırın gerçek
olduğunun kanıtı. Oradaki ders "yığına büyük şey koyma" değil,
"yığının ne kadar olduğunu bil" idi.

## Yalıtımın iki yönlü sınanması

Kriter 6 iki ayrı iddia ve ikisi de ayrı ayrı gösteriliyor, çünkü
birini göstermek diğerini göstermiyor.

**Klavye ekranı göremiyor.** Klavye bileşeni, konsolun framebuffer'ı
eşlediği adrese (`FRAMEBUFFER_BASE`) yazmayı deniyor. Beklenen: #PF.
Kendi VSpace'inde o adreste bir eşleme yok ve eşleme yapacak bir
capability'si de yok.

**Konsol klavyeyi süremiyor.** Konsol, tutmadığı bir slot üzerinden
`IOPort.In8(0x60)` deniyor. Beklenen: `EMPTY_SLOT` — "reddedildi"
değil, "böyle bir capability yok". ADR 018'in inşa yoluyla yalıtım
dediği şey.

İkisinin farkı önemli: ilki donanım koruması (sayfa tablosu),
ikincisi capability modeli. Bir bileşen diğerine erişemiyorsa bunun
*hangi* mekanizma yüzünden olduğu, mekanizmalardan biri bozulduğunda
fark edilmesi için ayrı ayrı bilinmeli.

## Sırala: konsol önce

Root task konsolu kurup `Resume` ediyor, **sonra** klavyeyi.

Ters sıra, klavyenin ilk tuşu kimse dinlemezken göndermesi demek.
`Send` bir rendezvous (ADR 033), yani klavye bloke olurdu — ölümcül
değil ama ölçümü bozardı: ilk tuşun gecikmesi konsolun açılış
süresini içerirdi.

Konsolun hazır olduğu, M13'ün `ping_driver` deseniyle biliniyor:
root task konsola bir mesaj gönderiyor ve `Send`'in dönmesi konsolun
`Recv`'e ulaştığı anlamına geliyor. Süre tahmini yok, olay var
(ADR 023, K8).

### `kbd-isolation` aynı sıralamayı başka bir sebeple istiyor

> **Eklendi (M15, kod yazıldıktan sonra).** Bu bölüm "ölçümü
> bozardı" diyordu. İzolasyon senaryosunda sıralama ölçümle ilgili
> değil, **doğruluk** ile ilgili, ve bu farkı görmek bir koşu
> kaybına mal oldu.

O senaryoda konsol servis vermiyor: tutmadığı bir IOPort'u
adlandırıyor, `EmptySlot` alıyor, bir kez gönderiyor ve duruyor.
Klavyenin yarısı ise bir **#PF**, ve #PF koşuyu bitiriyor — kernel'in
fault kurtarma mekanizması yok, dönmek fault eden komutu sonsuza
kadar yeniden çalıştırmak demek.

Dolayısıyla klavye önce fault ederse konsolun reddi **hiç
raporlanmıyor**. Test eksik bir satır yüzünden düşüyor, oysa her iki
bileşen de tam istendiği gibi davranmış.

Bu yüzden root task, izolasyon senaryosunda klavyeyi `Resume`
etmeden önce konsolun mesajını **alıyor**. `Recv` bir rendezvous
(ADR 033): dönmesi, konsolun denemesini yapıp söylediği anlamına
geliyor. Yine süre değil, olay.

**Belirtisi yanıltıcıydı.** Log'da beklenen satır bir sonrakiyle
birleşmiş gibi görünüyordu, ve ilk hipotez M14'ün `print!`
araya girme yarışıydı. Değildi: satır bozulmamıştı, **hiç
basılmamıştı**. Tanıdık bir arızaya benzeyen bir arıza, tanıdık
olanın teşhisini davet ediyor; ayırt eden şey log'da satırın
*aranması* oldu — `grep` hiçbir şey bulmadı.

Arıza üç koşuda bir düşüyordu, ve tek bir yeşil tam tur onu
kapattığını sanmaya yetmişti. ADR 023'e K13 bunun için eklendi:
kararsız bir senaryo en az 30 ardışık temiz koşu ister. Düzeltmeden
sonra `scripts/soak.sh 40 run-kbdisolation` → 40/40.

## Elenen alternatifler

**(a) Notification ile bildirim, veri paylaşımlı bellekte.** Daha
hızlı olurdu. Reddedildi: paylaşımlı bellek iki bileşenin
yalıtımını zayıflatır ve M15'in iddiası tam da yalıtım. Ayrıca
ölçüm anlamını yitirirdi — tuş→piksel gecikmesinin IPC maliyetini
içermesi gerekiyor.

**(b) Klavyenin doğrudan framebuffer'a yazması.** Konsol bileşeni
olmazdı. Reddedildi: tek bileşende yalıtım iddiası test edilemez
hale gelir (ADR 023, K5).

**(c) Root task'ın aracı olması.** Klavye root task'a, root task
konsola. Fazladan bir IPC turu ve root task'ın sıcak yolda olması.
Reddedildi: root task kurucudur, veri yolunda değil.

## Sonuçlar

> **Düzeltme (M15, kod yazıldıktan sonra).** Bu bölümün ilk hâli
> gövdeyle çelişiyordu: konsola "bir sıradan Untyped" veriyor ve
> CNode radix'ini 12 diye yazıyordu. İkisi de ADR 054'ün gölge
> tampon düzeltmesinden *önceki* tasarımın rakamları.
>
> Gövde (bu belgenin "iki bileşen de sıradan Untyped tutmuyor" ve
> "Konsolun CSpace'i: radix 11" başlıkları) o düzeltmeyle birlikte
> güncellendi; Sonuçlar güncellenmedi ve iki sayfa arayla kendi
> kendisiyle çelişen bir ADR ortaya çıktı.
>
> **Silinmedi, düzeltildi.** ADR 050'nin "muhasebe capability'de
> yaşar" düzeltmesiyle aynı desen: eski metin ne dediyse o
> kalıyor, yanında neyin neden değiştiği yazıyor. Bir ADR'nin
> değer verdiği şey vardığı sonuç değil, nasıl vardığı.
>
> | | eski Sonuçlar | gerçek |
> |---|---|---|
> | Konsolun sıradan Untyped'ı | "bir tane" | **yok** |
> | CNode radix | 12 (4096 slot) | **11 (2048 slot)** |
> | Gerekçe | gölge tampon için 1000 Frame daha | ızgara o Frame'leri ortadan kaldırdı |
>
> Nasıl yakalandı: kod okunarak, test koşturularak değil. `make
> kbd-test` radix 12 ile de geçerdi — fazladan 2048 slot bir
> CNode'u 128 KiB yapar ve hiçbir şeyi bozmaz. Yani bu çelişkiyi
> hiçbir test yakalayamazdı, ve bu ADR 023'ün K5'inin belge
> tarafındaki hâli: yanlışlığı gözlemlenemeyen bir cümle.

- `klavye --Endpoint(badge 0xKBD0)--> konsol`, tek yönlü.
- Klavye: iki IOPort, IRQHandler 1, Notification, Endpoint. Untyped
  yok.
- Konsol: framebuffer device Untyped'ları, PageTable'lar, Endpoint.
  **Sıradan Untyped yok**, IOPort yok, IRQ yok.
- **Hiçbir bileşen sıradan Untyped tutmuyor**, dolayısıyla hiçbiri
  çalışma zamanında nesne yaratamıyor. Konsolun tek ayrıcalığı
  framebuffer'ın device Untyped'ları, ki onlar yalnızca Frame
  olabilir (ADR 017).
- Konsolun CNode radix'i **11** (2048 slot): 1000 framebuffer
  Frame'i, 8 PageTable, ~10 sabit capability, ve pay.
  `roottask/src/keyboard.rs`'de `CONSOLE_CNODE_RADIX`.
- Konsolun yığını dört sayfa — 8000 baytlık karakter ızgarası
  `serve()`'ün yereli ve tek sayfaya sığmıyor. Bunun için
  `Component::allocate_sized` eklendi (`roottask/src/driver.rs`).
- Yalıtım iki yönde ayrı ayrı sınanıyor: biri #PF, diğeri
  `EMPTY_SLOT`.
- Konsol önce kurulup hazır olduğu doğrulanıyor, sonra klavye.
# ADR 032 — User tarafında W^X: `EXEC` bir hak, ve `WRITE` ile birlikte verilemez

Status: accepted (M8)

ADR 027 bu soruyu açıkça erteledi: "`EXEC` **verilmiyor**. M7'de
eşlenen her sayfa NX. Çalıştırılabilir user sayfaları M8'in işi, ve
W^X'in user tarafında ne anlama geldiği orada karara bağlanacak.
Şimdilik vermemek, sonra vermekten kolay."

M8'de ring 3 kodu çalışması gerekiyor, yani o kodun bulunduğu sayfa
çalıştırılabilir olmak zorunda. Borç ödeniyor.

## Kernel tarafındaki W^X ne diyordu

M3 kernel imajını bölüm bölüm eşledi: `.text` okunabilir ve
çalıştırılabilir ama yazılamaz; `.rodata` yalnızca okunabilir;
`.data` ve `.bss` okunabilir-yazılabilir ama NX. PMAP penceresi
tümüyle NX.

Kural: **hiçbir sayfa aynı anda yazılabilir ve çalıştırılabilir
değil.** Gerekçe standart — yazılabilir bir sayfaya kod enjekte edip
oraya atlamak, en eski sömürü kalıbı.

Kernel tarafında bunu uygulamak kolaydı, çünkü eşlemeleri kernel
kendisi yapıyor ve hangi bölümün ne olduğunu linker script biliyor.
User tarafında eşlemeyi **capability tutan** yapıyor, ve capability
tutan kernel'e güvenilir değil.

## `Rights::EXEC` diye bir hak yok

ADR 021 dört hak biti tanımladı: `READ`, `WRITE`, `GRANT`,
`GRANTREPLY`. Beşinci bir `EXEC` biti eklemek ilk akla gelen çözüm ve
reddedildi.

Sebep, hak bitlerinin ne olduğuyla ilgili. ADR 021'de bir hak,
**nesne üzerinde yapılabilecek bir işlem**: `READ` veri çıkarmak,
`WRITE` veri koymak, `GRANT` capability taşımak. "Çalıştırılabilir"
bir Frame'in özelliği değil — aynı Frame bir adres uzayında kod, bir
başkasında veri olabilir, ve ikisi de meşru.

Daha pratik bir sorun: `EXEC` bir hak olsaydı, ADR 021'in monotonik
daraltma kuralı onu da kapsardı — bir capability kopyalanırken haklar
yalnızca azalabilir. İyi görünüyor, ama ters yönde bir boşluk bırakıyor:
`READ|WRITE|EXEC` haklı bir Frame, hem yazılabilir hem çalıştırılabilir
eşlenebilirdi. Daraltma kuralı bunu *engellemez*, çünkü hiçbir hak
genişletilmiyor.

## Karar: `Map` bir izin argümanı alıyor, ve kombinasyon reddediliyor

`map_frame` artık bir `MapPermissions` argümanı alıyor:

```rust
pub enum MapPermissions {
    /// Okunabilir, yazılamaz, çalıştırılamaz.
    ReadOnly,
    /// Okunabilir ve yazılabilir, çalıştırılamaz.
    ReadWrite,
    /// Okunabilir ve çalıştırılabilir, yazılamaz.
    ReadExecute,
}
```

Üç varyant, ve **dördüncüsü yok.** "Okunabilir, yazılabilir ve
çalıştırılabilir" ifade edilemiyor — bir çalışma zamanı kontrolü
değil, tipte olmayan bir durum.

Bu, ADR 012'nin `TlbFlush` kararının aynı şekli: bir kural tipte
ifade edilebiliyorsa, yorum yazmak daha zayıf bir seçim. Bir
`PageFlags` bit maskesi alıp "WRITE ve EXEC birlikte mi" diye kontrol
etmek de çalışırdı — ve o kontrol silinebilir, unutulabilir, bir
refactor'da kaybolabilir. Bir enum varyantı silinemiyor; silinirse
build kırılıyor.

Haklar hâlâ üst sınır: `ReadWrite` istemek `Rights::WRITE` olmayan bir
capability ile `InsufficientRights` veriyor. Yani iki kapı var ve
ikisi de geçilmeli — capability ne verebileceğini, izin argümanı ne
istendiğini söylüyor.

### `ReadExecute` için `WRITE` hakkı gerekli mi

Hayır, ve bu düşünülmesi gereken bir noktaydı. İlk taslak
"`ReadExecute` için `Rights::READ` yeter" diyordu, ki doğru görünüyor:
çalıştırmak okumanın bir biçimi.

Ama bir sonuç var: salt-okunur bir Frame capability'si tutan biri onu
çalıştırılabilir eşleyebilir. Bu bir açık mı?

Değil, çünkü **içeriği değiştiremiyor**. Çalıştırılabilir yapmak,
orada zaten ne varsa onu çalıştırmak demek — ve oraya bir şey yazmak
için `WRITE` gerekiyor. Saldırgan senaryo "kod yaz, sonra çalıştır"
ve o iki adımın ikisi de aynı Frame'de gerekiyor; `WRITE` ile
eşleyip yazmak, sonra `Unmap` edip `ReadExecute` ile yeniden eşlemek
mümkün — ve işte bu, gerçek sınırın nerede olduğunu gösteriyor.

## Sınır: W^X *eşzamanlı*, kalıcı değil

Yukarıdaki senaryo — yaz, çöz, çalıştırılabilir yeniden eşle —
**engellenmiyor** ve engellenemez.

Bunu açıkça yazmak önemli, çünkü "W^X" ifadesi bazen daha güçlü bir
şey ima ediyor. Bu kernel'in sağladığı şey:

> Hiçbir sayfa, **aynı anda**, aynı adres uzayında hem yazılabilir
> hem çalıştırılabilir eşli değildir.

Sağlamadığı şey:

> Bir sayfanın içeriği, çalıştırılabilir hale geldikten sonra asla
> değişmemiştir.

İkincisi için ya bir imzalama mekanizması ya da "bir kez
çalıştırılabilir yapılan Frame bir daha yazılabilir eşlenemez"
şeklinde kalıcı bir durum gerekir. İkincisi Frame nesnesine bir bayrak
eklemek demek; mümkün ama M8'in kapsamında değil ve gerçek faydası
tartışmalı — JIT derleyiciler tam olarak bu kalıbı kullanıyor.

Bilinen eksiklere yazıldı. Önemli olan, sınırın belgelenmiş olması:
test ettiğimiz şey eşzamanlılık, ve iddia ettiğimiz şey de o.

İki adres uzayı arasında da aynı: aynı Frame, A'da `ReadWrite`, B'de
`ReadExecute` eşlenebilir. Capability'yi iki yere veren kişi bunu
bilerek yapıyor — ve `copy` ile hakları daraltmak onun elinde.

## SMEP: kernel'in user kodunu çalıştırması

W^X user kodunun kendi içindeki bir kural. Ayrı bir soru: **kernel**
bir user sayfasındaki koda atlayabilir mi?

CR4.SMEP M3'ten beri set ve tam olarak bunu engelliyor. Bir saldırgan
user belleğine kod koyup kernel'i oraya atlatabilirse (bir fonksiyon
işaretçisini bozarak, ROP ile), SMEP olmadan o kod CPL 0'da çalışır.

M3 SMEP'i etkinleştirdi ama **koruyacak bir user sayfası yoktu.**
`hardware-todo.md`'nin 4. maddesi tam olarak bunu söylüyordu: "Bitler
CR4'te, ama koruyacak bir user sayfası yok."

M8'de var. `user-smep` senaryosu kernel'den bir user sayfasındaki
koda atlıyor ve `#PF` bekliyor — error code'da **instruction-fetch
biti (bit 4)** set. Bu, M1'in beş bitlik `#PF` çözücüsünün son
kullanım alanı: M3'ün NX testi bit 4'ü bir *kernel* sayfasında
göstermişti, bu bir *user* sayfasında gösteriyor, ve ikisi farklı
mekanizmalar.

SMEP'in bir yan etkisi: ring 3'e ilk giriş için kullanılan `iretq`,
kernel kodundan user koduna atlıyor — ama atlama anında CPL zaten 3'e
düşüyor, yani SMEP uygulanmıyor. Sıralama önemli ve doğru: `iretq`
atomik.

## USER bayrağı ve `ReadExecute`

Bir ince nokta: user kod sayfası hem `USER` hem çalıştırılabilir olmak
zorunda, ve bu kombinasyon tam olarak SMEP'in izlediği şey. Yani aynı
sayfa:

- ring 3'ten çalıştırılabiliyor (USER set, NX temiz),
- ring 0'dan çalıştırılamıyor (SMEP, USER set).

İkisi çelişmiyor — SMEP'in tanımı bu. Ama test yazarken ayırt etmek
gerekiyor: `user-smep` senaryosunda `#PF` alınması, sayfanın
çalıştırılamaz olduğunu *göstermez*; ring 0'dan çalıştırılamadığını
gösterir. Aynı sayfanın ring 3'ten çalıştığı, pozitif testte zaten
kanıtlanıyor — user kodu çalışıyor.

Bu ayrımı yapmak için iki testin **aynı sayfayı** kullanması gerekiyor.
Farklı sayfalar kullanılsaydı, "biri çalışıyor, diğeri faultluyor"
gözlemi SMEP yerine bir eşleme farkıyla da açıklanabilirdi.

## Uygulama: `rights_to_flags` ikiye bölünüyor

M7'nin `rights_to_flags`'i haklardan bayrak üretiyordu ve `EXEC`'i hiç
vermiyordu. Artık iki girdi var — haklar ve istenen izin — ve çıktı
ikisinin kesişimi:

```rust
fn permissions_to_flags(
    rights: Rights,
    requested: MapPermissions,
) -> Result<PageFlags, CapError>
```

`READ` yoksa yine `InsufficientRights`. `ReadWrite` istenip `WRITE`
hakkı yoksa yine `InsufficientRights` — **sessizce salt-okunura
düşürülmüyor.** M7 bunu düşürüyordu ("WRITE yoksa RO eşle") ve o
davranış artık yanlış: çağıran yazılabilir bir eşleme istediyse ve
alamıyorsa, bunu bilmeli. Sessiz bir düşürme, ilk yazmada gelen bir
`#PF` olarak görünür ve sebebi eşleme çağrısında değil, çok sonrasında
aranır.

Bu bir **davranış değişikliği** ve M7'nin testini etkiliyor: eski test
"WRITE olmadan eşlenince RO oluyor" diye kontrol ediyordu. Artık
`ReadOnly` isteyip RO almak, ya da `ReadWrite` isteyip
`InsufficientRights` almak. Test güncellendi.

## Elenen alternatifler

**`Rights::EXEC` eklemek.** Yukarıda: haklar nesne üzerindeki
işlemler, "çalıştırılabilir" bir eşleme özelliği. Ayrıca daraltma
kuralı W^X'i sağlamazdı.

**`PageFlags` doğrudan almak.** `map_frame(root, frame, addr, flags)`.
Esnek, ve W^X'i bir çalışma zamanı kontrolüne indirger. Bir enum
varyantının olmaması, bir `if`'in olmamasından daha güçlü.

**Her user sayfasını NX bırakıp kodu kernel yarısından çalıştırmak.**
M7'nin durumu, ve ring 3 için işe yaramaz: kernel yarısı USER
bayrağı taşımıyor, yani CPL 3'ten erişilemez.

**Kalıcı W^X (bir kez EXEC olan Frame bir daha WRITE eşlenemez).**
Frame'e bir bayrak eklemek gerekirdi. Daha güçlü, ama JIT'i imkânsız
kılıyor ve M8'in kapsamında değil. Bilinen eksiklere yazıldı.

## Sonuç

- **`Rights::EXEC` yok.** Haklar nesne üzerindeki işlemler;
  çalıştırılabilirlik eşlemenin bir özelliği.
- `Map` bir `MapPermissions` alıyor: `ReadOnly`, `ReadWrite`,
  `ReadExecute`. **Dördüncü varyant yok** — W^X bir çalışma zamanı
  kontrolü değil, ifade edilemeyen bir durum.
- Haklar hâlâ üst sınır: `ReadWrite` istemek `WRITE` hakkı
  gerektiriyor, ve hak yoksa **sessizce RO'ya düşmüyor** —
  `InsufficientRights`. M7'nin davranışı bu noktada değişti.
- `ReadExecute` yalnızca `READ` gerektiriyor; içeriği değiştirmek
  için `WRITE` zaten gerekiyor.
- **Sağlanan şey eşzamanlı W^X**, kalıcı değil: yaz → çöz →
  çalıştırılabilir eşle dizisi mümkün ve engellenmiyor. Sınır
  belgelendi.
- **SMEP ilk kez gerçekten bir şey koruyor.** `hardware-todo.md`
  madde 4'ün QEMU tarafı kapanıyor. Test, ring 3'ten çalışan ve ring
  0'dan `#PF` veren **aynı sayfayı** kullanıyor — farklı sayfalar
  gözlemi bir eşleme farkıyla da açıklanabilirdi.
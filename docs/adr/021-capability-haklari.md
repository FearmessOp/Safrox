# ADR 021 — Haklar: dört bit, monoton daralma

Status: accepted (M5 architect pass)

Kaynak: seL4 Reference Manual bölüm 3.1.1 ("Capability Rights") ve
bölüm 4.2 (IPC'de cap transferi).

## Bağlam

Bir capability yalnızca "şu nesneye erişebilirsin" demez; **ne kadar**
erişebileceğini de söyler. Aynı Endpoint'e iki bileşenden biri yalnızca
gönderebilir, diğeri yalnızca alabilir — ikisi de aynı nesnenin cap'ine
sahiptir, hakları farklıdır.

Bu, yetki devri modelinin kalbi: bir bileşen sahip olduğu yetkiyi
başkasına verirken **daraltarak** verebilmeli. Yoksa her devir, tam
yetki devri olurdu ve "en az yetki" ilkesi uygulanamazdı.

## Seçim: dört hak biti

| Bit | Ad | Anlamı |
|---|---|---|
| 0 | `READ` | Nesneden veri al (Endpoint'ten receive, Frame'i okunabilir eşle) |
| 1 | `WRITE` | Nesneye veri koy (Endpoint'e send, Frame'i yazılabilir eşle) |
| 2 | `GRANT` | Bu nesne üzerinden **başka cap'ler** taşı |
| 3 | `GRANTREPLY` | Yalnızca cevap cap'i taşı |

seL4 ile aynı dörtlü. Anlamları nesneye göre değişiyor ve tam tablo
[docs/object-model.md](../object-model.md)'de.

### GRANT neden ayrı bir hak

`WRITE` bir Endpoint'e **veri** göndermeyi sağlar. `GRANT`, o mesajın
içinde **capability** taşımayı sağlar.

Fark kritik: veri göndermek, alıcının zaten sahip olduğu yetkiyi
kullanması demek. Cap göndermek, alıcıya **yeni bir yetki vermek**
demek. Biri bilgi akışı, diğeri yetki akışı.

Bu ayrım olmadan, bir Endpoint'e mesaj gönderebilen herkes alıcıya
istediği yetkiyi verebilirdi. Bir "log servisi"ne yazabilen bir bileşen,
ona bir Untyped cap'i geçirip sistemin bellek bütçesini bozabilirdi.

**Soruya doğrudan cevap: hayır.** `GRANT` hakkı olmayan bir cap ile IPC
yapılabilir ama o mesaj cap taşıyamaz. Taşımaya kalkarsa kernel cap
alanlarını **sessizce boş bırakır** — hata vermez.

İki gerekçe, ikisi de mütevazı:

1. **IPC sıcak yolunda bir kontrol daha istemiyoruz.** Cap alanlarının
   dolu olup olmadığına bakıp hakla karşılaştırmak, her mesajda bir dal.
   Düşürme zaten yapılması gereken işin bir parçası.
2. **seL4 de böyle yapıyor.** Kanıtlanmış bir sistemle uyumsuz
   davranmak için bir sebep gerekir; burada yok.

*Alternatif:* `InsufficientRights` dönmek. Hata ayıklamayı
kolaylaştırırdı. Yukarıdaki iki gerekçe ağır bastı — ama bunlar "kesin"
değil, "yeterli" gerekçeler, ve M9'da IPC yazılırken yeniden
bakılabilir.

> **Düzeltme notu (M5 uygulama).** Bu bölümün ilk hâli şunu iddia
> ediyordu: *"hata döndürmek, gönderene alıcının protokolü hakkında
> bilgi sızdırırdı."* Yanlış. Gönderen kendi haklarını zaten biliyor;
> `GRANT`'i olmadığını kernel'e sormasına gerek yok, kendi cap'ine
> bakar. Sızan bir bilgi yok. Karar aynı kalıyor ama gerekçe
> değiştirildi — zayıf bir gerekçe, sonradan yanlış bir yerde
> savunulur.

### GRANTREPLY: neden dördüncü bir bit

Çağrı-cevap deseninde (client `call`, server `reply`) sunucunun
istemciye cevap verebilmesi gerekir. Bu, sunucunun eline geçici bir
"cevap hakkı" verilmesi demek.

`GRANT` bunu sağlardı ama fazlasını da sağlar: tam GRANT'li bir cap,
sunucuya istemciye *herhangi bir* cap gönderme yetkisi verir.
`GRANTREPLY`, yalnızca cevap cap'inin taşınmasına izin verir.

Tipik kullanım: bir istemci sunucuya `WRITE | GRANTREPLY`'li bir
Endpoint cap'i tutar. Çağrı yapabilir, cevap alabilir, ama sunucuya
yetki devredemez.

[ADR 017](017-nesne-kumesi.md)'de Reply'i ayrı bir nesne yapmadığımızı
yazmıştım; cevap hakkı TCB'nin içinde örtük duruyor. `GRANTREPLY` o
örtük hakkın cap tarafındaki karşılığı.

## Monoton daralma: haklar asla genişleyemez

**Değişmez:** bir cap'in hakları, türediği cap'in haklarının alt
kümesidir. İstisnasız.

```
copy(src, dest, new_rights):
    if new_rights & !src.rights != 0:
        return RightsEscalation
    dest.rights = new_rights
```

Yani `diminish` ayrı bir işlem değil — `copy`'nin bir parametresi.
seL4'te de böyle (`CNode_Copy` bir hak maskesi alır), ve ayrı bir
`CNode_Mint` işlemi hem hak daraltma hem badge ekleme yapar.

Bu değişmez, derivation ağacının tamamı için geçerli: 8 seviye aşağıdaki
bir cap, kökün haklarından fazlasına sahip olamaz. Kontrol her `copy`'de
tek bir maskeleme — ağacı yürümeye gerek yok, çünkü her adımda
korunuyorsa zincirin tamamında korunur. **Tümevarım, çalışma zamanı
kontrolü değil.**

### Mevcut bir cap'in hakları daraltılabilir mi?

**Hayır.** `copy` yeni bir slot'a yazar; mevcut bir slot'un haklarını
yerinde değiştiren bir işlem yok.

Gerekçe: yerinde daraltma, o cap'in *çocuklarını* da etkilemeli olurdu
(yoksa değişmez bozulur) — yani ağaç yürüyüşü, yani sınırsız iş. Yetkiyi
geri almak isteyenin aracı `revoke`.

## Badge: taşıyıcı kimliği

Haklar dışında bir alan daha: **badge**, 64 bitlik bir etiket.

Bir Endpoint cap'ine badge takılabilir. O cap ile gönderilen her mesaj,
alıcıya badge'i de iletir. Sunucu böylece "bu istek hangi istemciden"
sorusunu cevaplar — istemcinin söylemesine gerek kalmadan, çünkü badge'i
kernel taşıyor ve istemci değiştiremiyor.

Kurallar:

- Badge yalnızca **badge'siz** (0) bir cap'e takılabilir. Bir kez
  takıldı mı değişmez.
- Badge takmak `GRANT` hakkı gerektirir — badge takmak, cap'i farklı bir
  nesneymiş gibi davranmaya sokar ve bu bir yetki üretimi sayılır.
- Badge'li bir cap'ten kopya alınabilir; kopya aynı badge'i taşır.

seL4 bunu `CNode_Mint` ile yapıyor; biz de `mint(src, dest, rights,
badge)` diye ayrı bir işlem tutuyoruz — `copy`'den ayrı, çünkü badge
takmak haklar gibi monoton daralan bir şey değil.

### Badge neden Notification'da da var

Bir Notification'a badge'li cap ile sinyal göndermek, badge bitlerini
Notification'ın bit maskesine **OR**'lar. Böylece bir Notification,
birden çok sinyal kaynağını tek bir bekleme noktasında toplar ve
uyanan thread hangi kaynaktan geldiğini maskeden okur.

M12'de bir cihaz sürücüsünün "IRQ mi geldi, yoksa istemci mi istedi"
sorusunu cevaplaması bu mekanizmayla olacak.

## Haklar nesneye göre ne anlama geliyor

Tam tablo [object-model.md](../object-model.md)'de; özet:

| Nesne | READ | WRITE | GRANT | GRANTREPLY |
|---|---|---|---|---|
| Endpoint | receive | send | cap taşı | cevap cap'i taşı |
| Notification | wait | signal | — | — |
| Frame | okunabilir eşle | yazılabilir eşle | — | — |
| CNode | slot oku (copy kaynağı) | slot yaz (copy hedefi) | — | — |
| Untyped | — | retype | — | — |
| TCB | durum oku | durum yaz, başlat/durdur | — | — |
| PageTable | — | eşle/çöz | — | — |
| IRQHandler | — | Notification'a bağla, ack | — | — |

Boş hücreler: o hakkın o nesnede anlamı yok. Böyle bir hak taşıyan bir
cap yaratılabilir (daralma kuralı ihlal edilmediği sürece) ama hiçbir
etkisi olmaz. Hata vermemesi bilinçli: anlamsız bir hak zararsız, ve
tip-başına geçerli hak maskesi tutmak bir tablo daha demek.

## Elenen alternatifler

**Tek bir "erişim" biti.** Endpoint'te gönderen ile alanı ayıramazdık,
Frame'de salt okunur eşleme yapamazdık. Capability modelinin faydasının
yarısı gider.

**Daha fazla hak biti (execute, delete, revoke...).** `EXECUTE` Frame
için cazip görünüyor ama eşleme izni zaten `PageFlags`'te
([ADR 011](011-sayfa-tablosu-api.md)) ve W^X orada zorlanıyor; cap
tarafında tekrar etmek iki doğruluk kaynağı demek. `DELETE` ve `REVOKE`
hakları: bir cap'e sahip olmak onu silebilmek demektir — kendi
slot'unuzdaki bir şeyi silmenizi engellemenin anlamı yok.

**Hakları ayrı bir tabloda tutmak.** Slot'ta 4 bit zaten boş duruyor
(tip alanının yanında). Ayrı tablo, bir dolaylılık daha.

**Yerinde hak daraltma (`diminish_in_place`).** Çocukları da etkilemesi
gerekirdi → ağaç yürüyüşü → sınırsız iş. `revoke` + yeni `copy` aynı işi
sınırlı maliyetle yapıyor.

**Badge'i değiştirilebilir yapmak.** Sunucunun istemci kimliğine
güvenmesi, badge'in değişmezliğine dayanıyor. Değiştirilebilir bir badge,
kimlik doğrulamayı yok eder.

## Sonuç

- Dört hak biti: `READ`, `WRITE`, `GRANT`, `GRANTREPLY`. seL4 ile aynı.
- **Haklar monoton daralır.** `copy` bir hak maskesi alır ve genişletmeyi
  reddeder; değişmez tümevarımla korunuyor, çalışma zamanı ağaç
  yürüyüşü yok.
- Yerinde daraltma yok. Yetki geri almanın yolu `revoke`.
- `GRANT` olmadan IPC cap taşıyamaz; cap alanları **sessizce boş kalır**,
  hata dönmez. Gerekçe: sıcak yolda bir dal daha istemiyoruz, ve seL4
  ile uyum. (İlk yazılan "bilgi sızıntısı" gerekçesi yanlıştı ve
  düzeltildi.)
- `GRANTREPLY`, cevap hakkını tam GRANT vermeden taşıtıyor.
- `mint` badge takar; badge bir kez takılır, değişmez, `GRANT` gerektirir.
- Anlamsız hak kombinasyonları hata değil, etkisiz.
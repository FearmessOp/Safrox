# ADR 037 — Badge: 32 bit, slot'ta, ve bir kez

Status: accepted (M9 architect pass). ADR 021'in badge bölümünü
somutlaştırıyor.

Kaynak: seL4 Reference Manual (v13), bölüm 4.2.2 ("Capability
Badges") ve `CNode_Mint`.

## Problem

Bir sunucu, üç istemciye aynı Endpoint'in cap'ini verdi. Bir mesaj
geldi. **Hangisinden?**

İstemcinin mesajda söylemesi işe yaramaz: yalan söyleyebilir. İkinci
istemci, birincinin kimliğini kullanarak onun verisini okuyabilir.

Badge bu soruyu kernel'in cevaplamasını sağlıyor: cap'e takılı bir
etiket, mesajla birlikte alıcıya iletiliyor, ve **istemci
değiştiremiyor**.

## Karar: 32 bit, slot'ta

ADR 021 badge'i 64 bit olarak tanımladı. Burada *nerede durduğu*
karara bağlanıyor.

Badge **capability slot'unda** duruyor — nesnede değil. Aynı
Endpoint'e üç farklı badge'li üç cap olabilir, ve zaten mesele bu.

ADR 019'un slot düzeni 32 bayt:

```
Slot (32 bayt):
    +0   object: u64        -- nesnenin fiziksel adresi
    +8   type/rights/depth/generation  (8 bayt)
    +16  derivation links   (16 bayt)
```

Badge için yer yok. **Çözüm: `extra` alanını kullanmak.**

M5'ten beri slot'ta bir `extra: u32` var ve nesne tipine göre anlam
değiştiriyor: CNode'da radix, Untyped'da `size_bits`. Endpoint ve
Notification için **badge** olacak.

### Ama `extra` 32 bit, badge 64 bit

Bu bir çelişki ve çözülmesi gerekiyor. Üç seçenek:

**(a) Badge'i 32 bite indir.** Dört milyar istemci kimliği. M13 için
fazlasıyla yeterli. Slot düzeni değişmiyor.

**(b) Slot'u 48 bayta çıkar.** ADR 017'nin 32 bayt tavanını deler,
ve her CNode'un boyutu %50 artar.

**(c) Badge'i nesnede tut.** Endpoint 32 bayt (ADR 017) ve üç alanı
dolu; badge nesnede olursa **cap başına farklı badge mümkün
olmazdı** — ki badge'in bütün amacı bu.

**Karar: (a). Badge 32 bit.**

ADR 021'in "64 bitlik bir etiket" ifadesi **düzeltiliyor.** O ADR
mimari geçişte yazıldı ve slot düzeni henüz ölçülmemişti; şimdi
ölçüldü ve sığmıyor.

Dört milyar yeterli mi: M13'ün init sistemi onlarca bileşen
başlatacak, binlerce değil. Bir servis her istemciye farklı badge
verse, 2^32 istemci sınırı pratikte sınırsız.

**Kapattığımız kapı:** badge'i yapılandırılmış bir değer olarak
kullanmak — örneğin üst 32 bit "istemci sınıfı", alt 32 bit "istemci
numarası". 32 bitte böyle bir bölüşüm daha sıkışık olur ama yine
mümkün (16+16).

## `extra`'nın üç anlamı ve ham erişimin yasaklanması

Karar (a) ile `extra` alanı üçüncü anlamını kazanıyor:

| Nesne tipi | `extra`'nın anlamı | Nereden |
|---|---|---|
| CNode | `radix` — kaç bit indeks | ADR 018 |
| Untyped | `size_bits` | ADR 020 |
| Endpoint, Notification | **badge** | bu ADR |

> **Karar (M9 uygulama kararı 4).** Üç anlamlı bir alan kabul
> edildi, **ama ham `extra()` çağrısı kod tabanında yasak.**
>
> Ayrım şurada: üç anlam taşıyan bir alan sorun değil — tip bilgisi
> slot'ta duruyor, yani hangi anlamın geçerli olduğu her zaman
> belirli. Sorun, o alanın **ayırt edilmeden okunabilmesi.** Bir
> `slot.extra()` çağrısının ne döndürdüğü, çağrı yerine bakılmadan
> anlaşılmıyor; ve bir CNode slot'undan badge okuyan bir kod derlenir,
> çalışır, ve yanlış cevap verir.
>
> Yerine **tip kontrollü erişimciler**:
>
> ```rust
> slot.as_badge()      // Endpoint | Notification degilse panic
> slot.as_radix()      // CNode degilse panic
> slot.as_size_bits()  // Untyped degilse panic
> ```
>
> Her biri tipi `assert` ediyor, ve yanlış tipte **panic** —
> `Err` değil. Gerekçe: bu bir kullanıcı hatası değil, bir kernel
> hatası. Yanlış tipten badge okumak, ADR 023'ün "tanımsız durum
> yok" kuralının ihlali, ve sessizce bir sayı döndürmek o ihlali
> gizlerdi.
>
> Ham `extra()` `pub` olmaktan çıkıyor; yalnızca üç erişimci ve
> `retype`/`mint` yazıcıları görüyor. Böylece yanlış okuma bir
> *derleme hatası* oluyor — en iyi hata türü.

### Neden `Option` değil, panic

`as_badge()` bir `Option<u32>` döndürebilirdi ve çağıran kontrol
ederdi. Reddedildi: her çağrı yerine bir `unwrap` ya da bir
`match` eklerdi, ve pratikte hepsi `unwrap` olurdu — yani aynı
panic, daha gürültülü bir yoldan.

Erişimcinin bir Endpoint slot'una uygulandığı, çağrı yerinde zaten
biliniyor: `invoke`, tipi çözmüş durumda. Bilinmediği bir yer varsa
orası bir tasarım hatası, ve panic onu gösteriyor.

## Badge 0 özel

Badge **0, "badge yok" demek.** Badge'siz bir cap ile gönderilen
mesajda alıcı `badge = 0` görür.

Bu, "badge 0 olan bir istemci" ile "badge'siz bir cap"i ayırt
edilemez kılıyor. Kabul ediliyor: sunucu badge kullanıyorsa, 0'ı
geçerli bir kimlik olarak atamaz.

Alternatif, bir "badge var mı" biti tutmaktı. Slot'ta yer yok ve
kazanç küçük.

## Bir kez takılır, değişmez

ADR 021'in kuralı, burada gerekçesiyle:

```
mint(src, dest, rights, badge):
    src.badge != 0      -> BadgeAlreadySet
    badge == 0          -> InvalidArgument (anlamsiz)
    src.rights.GRANT yok -> InsufficientRights
    dest dolu           -> SlotNotEmpty
    rights ⊄ src.rights -> RightsEscalation
    -> dest = src, badge takili, haklar daraltilmis
```

### Neden değiştirilemez

**Sunucunun istemci kimliğine güvenmesi, badge'in değişmezliğine
dayanıyor.**

Değiştirilebilir olsaydı: istemci A, badge'ini B'ninkiyle değiştirip
B'nin verisine erişirdi. Kimlik doğrulama diye bir şey kalmazdı.

Bu, ADR 021'in "badge'i değiştirilebilir yapmak" alternatifini
reddetme gerekçesiydi ve aynen geçerli.

### Neden badge'li bir cap'ten yeni badge takılamaz

`src.badge != 0` kontrolü. Sebep: badge'li bir cap'ten farklı
badge'li bir kopya üretmek, kimlik üretmek demek.

İstemci A'nın badge'i 5. A, badge'i 7 olan bir kopya üretebilseydi,
B'nin (badge 7) kimliğini alırdı.

### Kopyalama badge'i koruyor

`copy` badge'e dokunmuyor: badge'li bir cap'in kopyası aynı badge'i
taşır.

Bu doğru davranış — bir istemci kendi cap'ini kopyalayıp iki
thread'inde kullanabilir, ve sunucu ikisini de aynı istemci olarak
görür. Kimlik bileşene ait, thread'e değil.

### `GRANT` neden gerekli

ADR 021: "badge takmak, cap'i farklı bir nesneymiş gibi davranmaya
sokar ve bu bir yetki üretimi sayılır."

Somut: badge'li cap, sunucu tarafından farklı muamele görür. Badge
takabilen, sunucunun gözünde yeni bir kimlik yaratıyor. `GRANT`
(yetki taşıma hakkı) bunun en yakın karşılığı.

## Alıcı badge'i nerede görüyor

ADR 034'ün dönüş düzeni: **RSI**.

```
Recv doner:
    RDI = message tag
    RSI = badge          <- burada
    R10, R8, R9 = mesajin ilk uc word'u
```

RSI, çağrıda method numarasını taşıyordu (ADR 030) ve dönüşte boşta.

### Badge her mesajda mı taşınıyor

Evet, ve maliyeti bir `mov`. Alıcı badge kullanmıyorsa yok sayar.

Alternatif, "badge istiyorum" bayrağı olurdu — bir dal, bir bit, ve
kazanç sıfır.

## Notification'da badge farklı çalışıyor

ADR 021 bunu yazmıştı: bir Notification'a badge'li cap ile sinyal
göndermek, badge bitlerini Notification'ın maskesine **OR**'lar.

Yani Endpoint'te badge bir *kimlik*, Notification'da bir *bit
maskesi*.

Fark neden: Notification asenkron ve birikimli (ADR 038). Beş sinyal
gelse beşi de birikir ve bekleyen thread hepsini bir kerede görür.
"Kim gönderdi" sorusunun cevabı tek bir değer olamaz — maske olmalı.

Sonucu: **Notification badge'leri genellikle tek bitlik** olur.
Badge 0x1 "IRQ geldi", 0x2 "istemci istedi", 0x4 "zaman aşımı".
Bekleyen thread maskeyi okur ve hangilerinin geldiğini görür.

32 bit, 32 ayrı sinyal kaynağı. M12'nin sürücüsü için fazlasıyla
yeterli.

## Elenen alternatifler

**Badge'i nesnede tutmak.** Cap başına farklı badge imkânsız
olurdu — badge'in bütün amacı.

**64 bit badge, slot'u büyüterek.** ADR 017'nin 32 bayt tavanını
deler, her CNode %50 büyür.

**Badge'i mesajın içinde taşımak.** İstemci yazardı, yani yalan
söyleyebilirdi. Kimlik doğrulama değil, kimlik beyanı olurdu.

**Değiştirilebilir badge.** Kimlik doğrulamayı yok eder.

**Badge'siz, "gönderen TCB'sinin adresi" ile kimlik.** Kernel'in
içsel bir adresini user'a sızdırırdı, ve bir thread'in TCB'si
değişince kimlik değişirdi — oysa kimlik bileşene ait olmalı.

## Sonuç

- Badge **32 bit**, capability slot'unun `extra` alanında.
  **ADR 021'in "64 bit" ifadesi düzeltildi**: slot 32 bayt ve
  64 bitlik bir alan daha sığmıyor.
- `extra` üç anlam taşıyor (radix, `size_bits`, badge) ve bu kabul
  edildi — **ama ham `extra()` yasak.** Yerine tipi `assert` eden
  üç erişimci: `as_badge()`, `as_radix()`, `as_size_bits()`.
  Yanlış tipte **panic**, çünkü bu bir kernel hatası. Üç anlamlı bir
  alan sorun değil; ayırt edilmeden okunması sorun.
- Dört milyar kimlik; M13'ün ihtiyacının kat kat üstünde.
  Kapattığımız kapı: badge'i geniş yapılandırılmış bir değer
  olarak kullanmak.
- **Badge 0 = badge yok.** Sunucu 0'ı geçerli kimlik olarak
  atamaz.
- `mint` ile takılır, **bir kez**: badge'li bir cap'ten farklı
  badge'li kopya üretilemez, yoksa kimlik üretilebilirdi.
- `GRANT` gerektiriyor — badge takmak sunucunun gözünde yeni bir
  kimlik yaratmak.
- `copy` badge'i korur: kimlik bileşene ait, thread'e değil.
- Alıcı badge'i **RSI**'da görüyor, her mesajda, koşulsuz.
- **Notification'da badge bir bit maskesi**, kimlik değil —
  sinyaller birikimli olduğu için tek bir değer yetmez. 32 ayrı
  sinyal kaynağı (ADR 038).
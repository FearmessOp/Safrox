# ADR 036 — Call ve Reply: cevap hakkı sunucunun TCB'sinde

Status: accepted (M9 architect pass)

Kaynak: seL4 Reference Manual (v13), bölüm 4.1 (`Call`/`ReplyRecv`)
ve 4.2.1. ADR 017 Reply'i ayrı bir nesne yapmamıştı ("TCB'nin içinde
örtük"); bu ADR o cümleyi somutlaştırıyor.

## Soru

`Call` = `Send` + cevap bekle. Peki sunucu kime cevap verecek, ve bu
hakkı nereden alıyor?

Üç yer mümkün:

**(a) Çağıranın TCB'sinde.** "Bana cevap bekleniyor" durumu ve
"kimden" işaretçisi.

**(b) Sunucunun TCB'sinde.** "Şuna cevap vereceğim" işaretçisi.

**(c) Ayrı bir Reply nesnesi.** seL4'ün MCS dalı böyle; ADR 017
reddetti.

## Karar: (b), sunucunun TCB'sinde

```
TCB (sunucu tarafi):
    reply_to: u64      -- cevap bekleyen cagiranin TCB fiziksel adresi, ya da 0
    reply_generation: u8  -- o TCB slot'unun generation'i
```

Çağıranın TCB'sinde ise yalnızca bir durum: `BlockedOnReply`
(ADR 033). **Hiçbir kuyrukta değil** — bir tek sunucunun
`reply_to` alanı onu gösteriyor.

### Neden (a) değil

Çağıranın TCB'sinde tutmak doğal görünüyor: "ben cevap bekliyorum".
Ama sunucu `Reply` dediğinde **kime** cevap vereceğini bulması
gerekir, ve o bilgi çağıranda duruyorsa sunucunun onu araması
gerekir.

Nereden? Endpoint'in kuyruğundan değil — çağıran randevudan sonra
kuyruktan çıktı. Bir tablo gerekirdi, ya da çağıran kuyrukta
kalmalıydı.

Kuyrukta kalmak daha kötü: o zaman bir sonraki `Recv`, cevap
bekleyen çağıranı yeni bir istek sanırdı.

### Neden (c) değil

ADR 017'nin gerekçesi aynen geçerli ve bir tane daha ekleniyor:
Reply nesnesi **bir Untyped'dan retype edilmeli**, yani her `Call`
bir allocation. Sıcak yolda.

seL4'ün MCS dalı bunu çözüyor — Reply nesnesini istemci önceden
ayırıyor ve tekrar kullanıyor — ama o zaman "istemci Reply nesnesini
unutursa ne olur" sorusu doğuyor ve cevabı bir yaşam döngüsü ADR'si
daha.

**Kapattığımız kapı:** cevap hakkının devredilebilmesi. Bir sunucu
"bu isteği başkası cevaplasın" diyemez, çünkü hak bir cap değil, bir
alan. ADR 017 bunu zaten yazmıştı.

## Akış

```
Istemci                  Kernel                    Sunucu
-------                  ------                    ------
Call(ep, msg)
                  randevu var mi? -----------> Recv(ep) bekliyor
                  mesaji kopyala
                  badge'i yaz
                  sunucu.reply_to = istemci
                  istemci.state = BlockedOnReply
                                            <-- Recv doner
                                                (isi yapar)
                                                Reply(msg)
                  reply_to gecerli mi?
                  mesaji kopyala
                  istemci.state = Ready
                  sunucu.reply_to = 0
Call doner    <--
```

Dikkat: `reply_to` **randevu anında** yazılıyor, `Recv` dönerken
değil. Sebep: `Recv` ile `Reply` arasında sunucu preempt edilebilir
ve başka bir thread aynı Endpoint'e `Recv` yapabilir — eğer
`reply_to` o anda yazılsaydı, ikinci thread birinci thread'in
çağıranını çalardı.

### `ReplyRecv`: tek syscall, iki iş

Bir sunucu döngüsü şöyle:

```
loop {
    Recv(ep)    -> istek al
    islem yap
    Reply(msg)  -> cevap ver
}
```

Bu iki syscall. `ReplyRecv(ep, msg)` ikisini birleştiriyor: önce
cevap ver, sonra bir sonraki isteği bekle.

Kazanç iki syscall yerine bir — M8'in ölçümüyle ~930 çevrim. Bir
sunucu döngüsünün yarısı.

`Reply` de ayrıca duruyor, çünkü bazen sunucu cevap verip başka bir
şey yapmak ister.

## Sunucu cevap vermeden ölürse: DoS vektörü

**Soru doğru soruldu ve cevabı önemli.**

Senaryo: istemci `Call` yaptı, `BlockedOnReply` durumunda. Sunucu
cevap vermeden:

- çöktü (bir `#GP` aldı ve TCB'si revoke edildi), ya da
- kasten cevap vermiyor (kötü niyetli), ya da
- sonsuz döngüde

İstemci sonsuza kadar bloke mi?

### Karar: `reply_to` geçersizleşince istemci uyanır, hata alır

İki durum ayrı ele alınıyor.

**Durum A: sunucunun TCB'si revoke edildi.**

Revoke sırasında kernel, ölen TCB'nin `reply_to` alanına bakıyor.
Sıfır değilse, o çağıranı uyandırıyor ve `ReplyTargetGone`
döndürüyor.

Maliyet: tek bir alan okuması ve en fazla bir thread'i uyandırma.
Sınırlı.

Bu, ADR 039'un konusu ve orada ayrıntılı.

**Durum B: sunucu yaşıyor ama cevap vermiyor.**

İstemci **sonsuza kadar bekler.** Ve bu kabul ediliyor.

Gerekçe iki parçalı, ve ikincisi daha önemli:

1. **Zaman aşımı bir zamanlayıcı özelliği.** "100 ms sonra vazgeç"
   demek için bir zamanlayıcı bütçesi gerekir — seL4'ün MCS dalının
   `SchedContext`'i. ADR 017 onu kapsam dışı bıraktı.

2. **Güvenmediğin bir sunucuya `Call` yapma.** Bu bir kural değil,
   modelin bir sonucu. `Call` yapmak için o Endpoint'in cap'ine
   sahip olmak gerekir, ve o cap birisi tarafından verilmiştir.
   Veren, alanın kime güvendiğine karar vermiştir.

   M13'ün init sistemi şu deseni kullanacak: bir bileşene yalnızca
   güvenilir servislerin Endpoint cap'leri veriliyor. Güvenilmeyen
   bir bileşenle konuşmak gerekiyorsa `Send` + `Recv` ayrı ayrı
   kullanılır — `Send` bloke olur ama `BlockedOnEndpoint`
   durumunda, ve oradan `Suspend` ile çıkarılabilir.

### Çıkış yolu var: `Suspend`

Bir `BlockedOnReply` thread'i **suspend edilebilir** (ADR 026:
`Suspend` her zaman çalışır). Suspend edilince:

- `reply_to` bağı kopar (sunucunun alanı temizlenir)
- thread `Suspended` durumuna geçer
- `Resume` edilirse `Call`'dan `ReplyTargetGone` ile döner

Yani bir denetleyici bileşen (M13'ün init sistemi) takılmış bir
istemciyi kurtarabilir. **Bu, sonsuz beklemenin bir sistem kilidi
olmamasının sebebi.**

Ama bunu yapabilmek için istemcinin TCB cap'ine sahip olmak gerekir
— yani kurtarma yetkisi de bir capability. Tutarlı.

### Kendi kendine `Call`

Bir thread kendi Endpoint'ine `Call` yaparsa? Kendisiyle randevu
olamaz (bir thread aynı anda hem gönderen hem alıcı olamaz), yani
`BlockedOnEndpoint` durumunda kalır — sonsuza kadar, kimse o
Endpoint'ten okumuyorsa.

Bu bir deadlock ve **engellenmiyor.** Engellemek, her `Send`'de
"alıcı ben miyim" kontrolü demek olurdu, ve o kontrol yalnızca
doğrudan kendine çağrıyı yakalar — A→B→A döngüsünü yakalamaz.
Kısmi bir koruma, tam maliyetle.

`Suspend` yine çıkış yolu.

## `reply_to` nasıl doğrulanıyor

Sunucu `Reply` dediğinde `reply_to` eski olabilir: çağıranın TCB'si
revoke edilmiş, yerine yeni bir TCB retype edilmiş, ve aynı adrese
düşmüş olabilir.

**Bu tam olarak ADR 028'in VSpace için çözdüğü problem**, ve çözüm
aynı: adres + generation.

```
reply_to geçerli mi:
    reply_to != 0
    ve o adresteki TCB'nin slot generation'i == reply_generation
    ve o TCB'nin durumu == BlockedOnReply
```

Üç karşılaştırma, döngü yok. Üçüncüsü de gerekli: TCB aynı olabilir
ama bu arada suspend edilip resume edilmiş olabilir, yani artık
cevap beklemiyor.

Geçersizse `Reply` **`ReplyTargetGone` döndürüyor** ve hiçbir şey
yapmıyor. Sunucu çökmüyor — ADR 030'un kuralı: her hata bir `Err`.

## Durum geçişleri

```
                Call(ep)
    Running ---------------> BlockedOnEndpoint
                                    |
                            randevu olustu
                                    v
                            BlockedOnReply
                               |        |
                    Reply geldi|        |Suspend / sunucu oldu
                               v        v
                            Running   Suspended / Ready(hata ile)
```

`Call` yapan thread **iki durumdan geçiyor**: önce randevu
beklerken `BlockedOnEndpoint`, randevu olunca `BlockedOnReply`.

İkisinin ayrı olması gerekiyor çünkü ilkinde thread Endpoint'in
kuyruğunda, ikincisinde hiçbir kuyrukta değil — ve iptal yolları
farklı (ADR 039).

## Teslimat nerede bekler (M9b'de düzeltildi)

Uyandırılan bir thread'e mesajı doğrudan verilemiyor: o anda
çalışmıyor, register'ları kayıtlı bir bağlamda. Mesajın bir yerde
beklemesi gerekiyor.

M9a bunu **tek bir global yuvada** yaptı, şu gerekçeyle: "aynı anda
en fazla bir teslimat havada olabilir, çünkü aktarım kesmeler kapalı
koşuyor ve uyandırılan thread başka hiçbir şey çalışmadan önce
mesajını okuyor."

Cümlenin ikinci yarısı yanlış. Bir thread'i uyandırmak
`sched::enqueue` çağırıyor; bu onu **hazır** yapıyor, çalıştırmıyor.
Uyandıran devam ediyor — ve cevap verip sonra tekrar alan bir sunucu
(ki `ReplyRecv`'in tamamı bu şekil) ilki hâlâ yuvadayken ikinci bir
teslimat park ediyor.

M9b'nin ilk koşusu ikinci senaryoda assertion'ı tetikledi. Assertion
görevini yaptı: iki istemcinin birbirinin cevabını almasına izin
vermek yerine bozuk bir invariant'ı bildirdi. Ama o invariant hiçbir
zaman **doğru** değildi — yalnızca erişilemezdi, çünkü M9a'da iki
teslimat hiç çakışmıyordu.

**Karar: teslimat hedef thread'in kendi TCB'sinde bekler**
(`Incoming`, 40 bayt). Gönderme yönü zaten böyle çalışıyordu
(`PendingSend`); asimetri bir karar değil, bir gözden kaçmaydı.
Assertion thread başına taşındı, dolayısıyla **yasal** durum — iki
farklı thread'e iki teslimat — artık tetiklemiyor.

Genel ders: "aynı anda yalnızca bir X olabilir" diyen bir global
durum, o cümlenin neden doğru olduğunu da yazmak zorunda. Burada
yazılmıştı, ve yazıldığı için yanlışlığı görülebildi.

## Temizlik mi, doğrulama mı: ikisi de, ama testleri ayrı

`reply_to` iki ayrı mekanizmayla korunuyor:

1. **Temizlik.** Çağıranın beklemesi iptal edilince `cancel_ipc`,
   `replying_server` ters bağını izleyip sunucunun `reply_to`'sunu
   yerinde siliyor (ADR 039).
2. **Doğrulama.** `reply_target_is_live` üç karşılaştırma yapıyor:
   yuva hâlâ bir TCB tutuyor mu, aynı nesneyi mi gösteriyor,
   generation oynadı mı.

ADR 028 bu deseni "kemer ve askı" diye kurmuştu. M9b bir şey daha
öğretti: **temizlik her zaman kazanınca, doğrulamanın kendi testi
gerekiyor.**

`ipc-stale-reply` tam da bunu yaşadı. Senaryo çağıranın TCB'sini
revoke ediyor, yerine aynı adrese yeni bir TCB koyuyor, ve sunucunun
`Reply`'inin reddedilmesini bekliyor. Reddediliyor — ama
`NoReplyTarget` ile, `ReplyTargetGone` ile değil: ters bağ `reply_to`'yu
çoktan silmişti, yani **üç karşılaştırmanın birincisi** cevabı
reddetti ve generation'a sıra hiç gelmedi.

Erişilebilir her yolda temizlik önce davranıyor. Dolayısıyla
senaryonun "generation eskimiş cevabı reddetti" demesi, doğru bir
sonucu yanlış bir gerekçeyle raporlamak olurdu.

**İki iddia ayrıldı:**

- **İddia 1 (canlı yol):** `Reply` `NoReplyTarget` veriyor, ve eski
  adresi işgal eden yabancı uyandırılmıyor — hâlâ `Inactive`.
- **İddia 2 (doğrulamanın kendisi):** `reply_target_is_live`
  doğrudan çağrılıyor; **aynı adreste** generation 1 kabul ediliyor,
  generation 0 reddediliyor. İki yarı da gerekli: yalnızca "eskisini
  reddediyor" demek, her şeyi reddeden bir kontrolde de geçerdi.

### K5 mutasyonu (koşuldu)

`reply_target_is_live`'dan `&& slot.generation() == generation`
satırı silindi. `ipc-stale-reply` öldü: İddia 2'nin eski yarısında
panik, exit 35, runner üç ihlal bildirdi. Satır geri kondu, senaryo
yeniden geçti.

İlk denemenin kendi bulgusu var. Derleme mutasyonu **reddetti** —
`-D warnings`, artık kullanılmayan `generation` bağını hataya
çevirdi, yani hiçbir şey koşmadı. Bu gerçek bir savunma hattı, ama
göründüğünden zayıf: bağı `_generation` yapmak onu tek hamlede
susturuyor, ki kontrolü makul görünen bir gerekçeyle kaldıran bir
düzenleme tam olarak bunu düşünmeden yapardı. Mutasyon o şekilde
tekrarlandı ki testin kendisi yakalamak zorunda kalsın — yakaladı.

Ders tek bir kontrolü aşıyor: "derleyici zaten yakalar", hatanın
**belirli bir yapılış biçimi** hakkında kanıttır, hatanın kendisi
hakkında değil.

## Elenen alternatifler

**Cevap hakkını çağıranın TCB'sinde tutmak.** Sunucunun çağıranı
bulması gerekirdi; ya bir tablo ya da çağıranın kuyrukta kalması.

**Ayrı Reply nesnesi.** Her `Call` bir allocation, ya da bir yaşam
döngüsü ADR'si daha. ADR 017 zaten reddetmişti.

**Zaman aşımı.** Bir zamanlayıcı bütçesi gerektirir (`SchedContext`,
ADR 017'de kapsam dışı). `Suspend` yeterli bir çıkış yolu.

**Deadlock tespiti.** A→B→A döngüsünü bulmak için bekleme grafiğini
dolaşmak gerekir — sınırsız iş, ve her `Call`'da.

**`Reply`'i bir capability yapmak.** Devredilebilir olurdu, ki
ADR 017 bunu bilinçli kapattı.

## Sonuç

- Cevap hakkı **sunucunun TCB'sinde**: `reply_to` (çağıranın TCB
  adresi) + `reply_generation`. Çağıranda yalnızca
  `BlockedOnReply` durumu, hiçbir kuyrukta değil.
- `reply_to` **randevu anında** yazılıyor, `Recv` dönerken değil —
  yoksa ikinci bir alıcı çağıranı çalabilirdi.
- `ReplyRecv` iki syscall'ı birleştiriyor. **Tasarruf M9b'de
  ölçüldü ve beklenenin çok altında çıktı: 13902'ye karşı 13776
  çevrim, yani ~126.** Gerekçesi mekanizmada değil, ölçümün
  koştuğu yerde: M13'e kadar user mode yok, dolayısıyla
  `reply_recv` bir Rust fonksiyon çağrısıyla erişiliyor ve hiçbir
  ayrıcalık geçişi tasarruf edilmiyor. ~930 çevrimlik iddia M13'te
  sınanabilir hale geliyor; o zamana kadar **iddia edilmiyor**,
  rapor ediliyor.
- Geçerlilik **üç karşılaştırma**: adres, generation, durum.
  ADR 028'in VSpace için kurduğu desenin aynısı.
- **Sunucu çökerse** çağıran uyanır ve `ReplyTargetGone` alır
  (ADR 039).
- **Sunucu kasten cevap vermezse çağıran sonsuza kadar bekler.**
  Kabul ediliyor: zaman aşımı bir zamanlayıcı bütçesi gerektirir
  (kapsam dışı), ve güvenilmeyen bir sunucuya `Call` yapmamak
  modelin bir sonucu.
- **Çıkış yolu `Suspend`** — her zaman çalışıyor (ADR 026), ve
  `reply_to` bağını koparıyor. Sonsuz bekleme bir sistem kilidi
  değil, ama kurtarma da bir capability gerektiriyor.
- Kendi kendine `Call` bir deadlock ve engellenmiyor; kısmi koruma
  tam maliyete değmez.
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
- `ReplyRecv` iki syscall'ı birleştiriyor; sunucu döngüsünün
  maliyetini yarıya indiriyor (~930 çevrim, M8 ölçümü).
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
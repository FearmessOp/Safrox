# ADR 035 — Capability transferi: en fazla dört, alıcının belirlediği yere

Status: accepted (M9 architect pass)

Kaynak: seL4 Reference Manual (v13), bölüm 4.2 ("Capability
Transfer") ve 4.2.2 (`CNode_SetIPCBuffer` / receive slot).

ADR 034 mesajın veri kısmını karara bağladı. Bu ADR yetki kısmını:
bir mesaj **capability** taşıdığında ne oluyor.

Ayrım ADR 021'in dediği gibi keskin: veri göndermek alıcının zaten
sahip olduğu yetkiyi kullanması, cap göndermek ona **yeni bir yetki
vermek**.

## Karar: en fazla dört cap

`caps` alanı 4 bit (ADR 034) ama geçerli aralık **0..4**.

Dört, bir ölçüm değil bir yargı. Gerekçe:

**Transfer sınırlı iş olmalı.** Her cap bir CPtr çözümlemesi
(ADR 018: ~20 komut), bir hak kontrolü, bir hedef slot kontrolü ve
bir derivation ağacı ekleme (ADR 019). Dördü, en kötü durumda IPC
yolunun maliyetini belirgin biçimde artırıyor ama sınırlı tutuyor.

> **Ölçüldü (M9c, `ipc-cap-cost`).** Yargının dayandığı "sınırlı
> iş" artık bir rakam — ve rakamın hangi yarısının güvenilir olduğu
> da bir bulgu:
>
> | Yük | Çevrim (tek başına) | Çevrim (suite içinde) |
> |---|---|---|
> | `Send`+`Recv`, 0 cap | 1302 | — |
> | 1 cap | 2058 | — |
> | 4 cap | 3360 | — |
> | **ilk cap** | 756 | **1806** |
> | **sonraki her cap** | ~434 | ~126 |
>
> **İlk cap'in rakamı ölçüm değil.** Hedef CNode'un slotlarına ilk
> dokunuşu — bir sayfa dolaşımı ve bir önbellek doldurma —
> taşıyor, ve örneklerin minimumu bunu temizleyemiyor: her örnek o
> slotları silip yeniden dolduruyor, yani ısınma *yolu* ısıtıyor,
> veriyi değil. Ne kadarının bu rakama düştüğü, öncesinde ne
> koştuğuna bağlı.
>
> İlk sürümde "dördüncü cap birincisi kadar mı" diye sorulmuştu ve
> assertion **kararsız** çıktı: tek başına 756/434 ile geçti,
> `make test` içinde 1806/126 ile düştü — 14 kat, süperdoğrusal bir
> maliyet diye raporlandı, ki öyle bir şey yoktu.
>
> Güvenilir olan ve ADR'nin savının gerçekten ilgilendiği rakam
> **marjinal** maliyet: ikinci cap ile dördüncü arasında büyüyor
> mu. Büyümüyor. Dört bir sınır, bir uçurum değil.
>
> Emülasyon şerhi: QEMU'nun TCG'si önbellek davranışını modellemiyor
> ve CPtr çözümlemeleri bir CNode dolaşıyor. Rakamlar bir **taban**.

**M13 için yeterli.** En karmaşık senaryo: init sistemi bir bileşen
başlatırken ona CSpace kökü, VSpace kökü, bir Endpoint ve bir
Notification veriyor — tam dört. Beşinci gerekiyorsa ikinci bir
mesaj, ki başlatma sıcak yol değil.

**Alternatif değerler ve neden değil:**

| Sayı | Neden değil |
|---|---|
| 1 | Bileşen başlatma iki mesaj olurdu; CSpace + VSpace neredeyse hep birlikte |
| 2 | Yukarıdaki dörtlü sığmaz |
| 8 | `caps` alanını büyütür; ve her ek slot, en kötü durumu uzatır |
| sınırsız | Mesaj başına iş sınırsız olur — master prompt yasağı |

## Alıcı cap'leri nereye koyar: `receive slot`

Alıcı, `Recv` çağrısından **önce** nereye alacağını söyler.

```
SetReceiveSlots(cnode_cptr, start_index, count)
```

Bu ayrı bir invocation, TCB üzerinde, ve TCB'de üç alan kaydediyor:
hedef CNode'un fiziksel adresi, başlangıç indeksi, kaç slot.

Gelen mesajdaki `caps` sayısı kadar cap, `start_index`'ten itibaren
ardışık slotlara yazılıyor.

### Neden mesajda belirtilmiyor

İlk tasarım, cap hedeflerini mesajın içinde taşımaktı: gönderen
"şunu şu slot'a koy" der. **Reddedildi**, ve sebep bir güvenlik
sorunu:

> Gönderen, alıcının CSpace'inde bir slot seçemez.

Seçebilseydi, bir istemci sunucuya bir cap gönderirken sunucunun
*zaten dolu olan* bir slot'unu hedefleyebilirdi. Hedef slot boşsa
yazılır, doluysa ne olur? Ya sunucunun cap'i silinir (yetki kaybı,
gönderen tarafından tetiklenen) ya da hata döner — ve o zaman
gönderen, sunucunun CSpace'inin hangi slotlarının dolu olduğunu
**tarayabilir**. İkisi de kabul edilemez.

Alıcının kendi slotunu seçmesi bu sorunu kökten kaldırıyor: hedef
alıcının bildiği, kendi seçtiği bir yer.

### Slot boş değilse

Alıcının gösterdiği slot doluysa, **o cap düşer** ve alıcının
gördüğü `caps` sayısı buna göre azalır.

Hata dönmüyor — ne gönderene ne alıcıya. Gerekçesi ADR 021'in
`GRANT` kararıyla aynı şekil:

- **Gönderene hata dönmek yanlış olurdu.** Gönderen, alıcının
  CSpace'i hakkında bir şey öğrenirdi. Tam olarak yukarıda
  engellediğimiz tarama.
- **Alıcıya hata dönmek gereksiz.** Alıcı zaten `caps` alanından
  kaç cap aldığını görüyor; beklediğinden az ise kendi slotlarının
  dolu olduğunu kendisi bilir — orası kendi CSpace'i.

Yani: alıcı `caps = 3` bekliyorken `caps = 1` görürse, iki slotunun
dolu olduğunu anlar. Bu bilgi zaten onun.

### Receive slot ayarlanmamışsa

`SetReceiveSlots` hiç çağrılmamışsa `count = 0`, yani **hiçbir cap
alınmaz.**

Bu, "varsayılan güvenli" demekten daha güçlü bir şey — ve ayrım
önemli olduğu için açıkça yazılıyor:

> **Yetkiyi almak bir eylemdir.** Bir cap, alıcının *önceden açtığı*
> bir slot'a düşer. Slot açılmamışsa cap gelmez; bu bir politika
> tercihi değil, mekanizmanın yapısı.

"Varsayılan güvenli" bir ayardır: değiştirilebilir, unutulabilir,
yanlış kurulabilir. Burada ise gönderenin elinde **hiçbir zaman**
alıcıya cap dayatacak bir araç yok — hedef adresini alıcı yazıyor,
gönderen o alanı göremiyor bile.

Pratikteki sonucu, aşağıdaki (asimetri) bölümünde: bir sunucu
istemciye istenmeyen bir cap — örneğin bütçesini karıştıracak bir
Untyped — gönderemez. Gönderebileceğini sanabilir, `Reply`'sinde
`caps = 1` yazar, ve **o cap sessizce düşer.** İstemci kabul etmeye
hazır değilse gelmez.

## Transferin kuralları

Her cap için, sırayla:

```
1. Gonderenin CPtr'ini coz        -> hata varsa bu cap duser
2. Gonderenin GRANT hakki var mi  -> yoksa bu cap duser (ADR 021)
3. Hedef slot bos mu              -> degilse bu cap duser
4. Derivation derinligi < 8 mi    -> degilse bu cap duser (ADR 019)
5. Kopyala, haklari degistirmeden
```

**Her adım sessizce düşürüyor.** Bir cap'in düşmesi diğerlerini
etkilemiyor — üçüncü cap düşerse dördüncü yine denenir.

### Haklar değişmiyor

Transfer edilen cap, gönderenin cap'iyle **aynı haklara** sahip.
Daraltma isteyen, göndermeden önce `copy` ile daraltılmış bir kopya
hazırlar (ADR 021: `copy` bir hak maskesi alır).

Alternatif, mesajda hak maskeleri taşımaktı. Mesaj formatını
büyütürdü ve `copy` zaten var.

### GRANTREPLY yeterli mi

**Hayır** — `GRANTREPLY` yalnızca cevap capability'sinin taşınmasına
izin veriyor (ADR 021), sıradan cap transferine değil.

Yani bir istemci `WRITE | GRANTREPLY`'li bir Endpoint cap'i ile:

- `Call` yapabilir, sunucu ona cevap verebilir ✓
- Mesajında cap taşıyamaz ✗ (düşer)

Bu tam olarak ADR 021'in tarif ettiği tipik kullanım.

## Derivation ağacı ve IPC

Transfer edilen cap, gönderenin cap'inin **çocuğu** olur (ADR 019).

Sonucu: gönderen `revoke` ederse, gönderdiği cap de gider. Bu
doğru davranış — yetkiyi veren geri alabilmeli.

Derinlik sınırı (8) burada da geçerli. Sekiz kez el değiştirmiş bir
cap dokuzuncuda düşer. M13'ün senaryosunda derinlik 2-3 olacak:
init → servis → istemci.

**Kapattığımız kapı:** uzun delegasyon zincirleri. Bir "yetki
aracısı" deseni (A, B'ye verir, B C'ye, C D'ye...) dokuz adımda
tıkanır.

## İki yönlü mi

**Hayır.** Cap transferi tek yönlü: `Send` ve `Call` yapanın
mesajında.

`Reply` **cap taşıyabilir mi?** Evet — sunucu istemciye cap
döndürebilir, ve bu M13 için gerekli: bir "servis bul" isteğine
cevap olarak servisin Endpoint cap'i dönüyor.

Reply'in `GRANT` kontrolü, sunucunun **reply hakkında** değil,
sunucunun gönderdiği cap üzerinde — çünkü reply hakkı bir Endpoint
cap'i değil (ADR 036). Yani sunucu, elindeki cap'i gönderebiliyorsa
gönderir.

Bu bir asimetri ve farkında olarak bırakılıyor: `Call` yapan `GRANT`
ister, `Reply` yapan istemez.

> **Karar (M9 uygulama kararı 2).** Açık soru 5'te bu asimetrinin bir
> sömürü yolu olup olmadığını sormuştum: `GRANTREPLY` veren bir
> istemci "bana cevap ver" diyor, "bana istediğin cap'i ver"
> demiyor. Bir sunucu istemciye istenmeyen bir Untyped gönderip
> bütçesini karıştırabilir mi?
>
> **Hayır, ve gerekçe `SetReceiveSlots`'un bir savunma olması
> değil** — ilk cevabım buydu ve zayıftı. Bir savunma aşılabilir;
> burada aşılacak bir şey yok.
>
> Yetkiyi almak bir **eylem**. Cap, alıcının önceden açtığı bir
> slot'a düşer. İstemci `SetReceiveSlots` çağırmadıysa sunucunun
> gönderdiği cap hiçbir yere gitmez — sunucunun elinde onu
> zorlayacak bir mekanizma yok. Mesele "varsayılan kapalı" değil,
> **yapısal olarak zorlanamaz.**
>
> Fark şurada görünüyor: bir varsayılan, yanlış kurulabilir. Bir
> yapı, kurulamaz. Alıcı slot açmadıysa transfer edilecek bir hedef
> *yoktur*, ve kodda bu bir `if count == 0 { drop }` değil, döngünün
> hiç dönmemesi.
>
> İkinci bir hak biti (`GRANTREPLY`'ye "cevapta cap kabul ederim"
> anlamı eklemek) bu yüzden gereksiz: bir hak bitinin anlamını ikiye
> bölerdi, ve koruduğu şey zaten korunuyor.
>
> Ve `SetReceiveSlots` çağıran bir istemci, cap kabul etmeye **hazır
> olduğunu söylemiş** oluyor. Ne geldiğine `caps` alanından bakar,
> istemiyorsa `delete` eder. Bu noktada bilinçli bir kabul var.

## Elenen alternatifler

**Cap hedeflerini mesajda taşımak.** Yukarıda: gönderene alıcının
CSpace'ini tarama imkânı verir.

**`Reply` için üçüncü bir hak biti.** `GRANTREPLY`'ye "cevapta cap
kabul ederim" anlamı eklemek, ya da yeni bir bit. Reddedildi:
koruyacağı şeyi slot mekanizması zaten koruyor, ve bir hak bitinin
anlamını ikiye bölmek onu okunamaz yapardı.

**Dolu slot'ta hata dönmek.** Aynı tarama sorunu, gönderene
dönerse. Alıcıya dönmek gereksiz çünkü bilgi zaten onda.

**Transferde hak daraltma.** `copy` zaten var; mesaj formatını
büyütmeye değmez.

**Sınırsız cap sayısı.** Mesaj başına iş sınırsız olurdu.

**Tek cap.** Bileşen başlatmayı iki mesaja bölerdi; dördün maliyeti
kabul edilebilir.

## Sonuç

- **En fazla dört capability** bir mesajda. Sayı, M13'ün bileşen
  başlatma senaryosundan geliyor: CSpace, VSpace, Endpoint,
  Notification.
- Hedef **alıcının seçtiği** ardışık slotlar (`SetReceiveSlots`).
  Gönderen hedef seçemez — seçebilseydi alıcının CSpace'ini
  tarayabilirdi.
- Ayarlanmamışsa hiçbir cap alınmaz. Bu bir varsayılan değil, bir
  **yapı**: yetkiyi almak bir eylem, ve gönderenin elinde onu
  dayatacak bir araç yok. Varsayılanlar yanlış kurulabilir; bu
  kurulamaz.
- Beş kontrol, her biri **sessizce düşürüyor**: CPtr geçersiz,
  `GRANT` yok, hedef dolu, derinlik aşıldı. Bir cap'in düşmesi
  diğerlerini etkilemiyor. Beşi de M9c'de ayrı ayrı tetikleniyor
  (`ipc-cap-test` 1–6, ADR 023'ün K5'i), ve "cap düştü" ile "mesaj
  geçti" **ayrı** iddialar olarak yazılıyor: mesajın tamamını
  reddeden bir kernel, yalnızca birincisine bakan bir testi geçerdi.
  Dolu slot senaryosunda engel **ortada** — ilkte olsaydı, ilk
  engelde pes eden bir uygulama da geçerdi.
- **Maliyeti ölçüldü** (`ipc-cap-cost`): marjinal maliyet cap
  sayısıyla büyümüyor, yani dört bir sınır — bir uçurum değil. İlk
  cap'in rakamı koşudan koşuya değişiyor (756 ↔ 1806) çünkü hedef
  slotlara ilk dokunuşu taşıyor; o yüzden iddia marjinal maliyet
  üzerine kurulu, ilk cap üzerine değil.
- Hata dönmemenin gerekçesi: gönderene dönmek tarama imkânı verir,
  alıcıya dönmek gereksiz — `caps` sayısı zaten söylüyor.
- Haklar transferde **değişmiyor**. Daraltma `copy` ile, önceden.
- Transfer edilen cap gönderenin **çocuğu** olur; gönderen revoke
  ederse gider. Derinlik sınırı 8 (ADR 019) burada da geçerli.
  Kapattığımız kapı: uzun delegasyon zincirleri.
- `Reply` cap taşıyabilir ve `GRANT` kontrolü sunucunun kendi
  cap'inde. Asimetri **kabul edildi**: sunucu istemciye cap
  dayatamaz, çünkü hedef slot'u istemci açar. Üçüncü bir hak biti
  gereksiz.
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
alınmaz.** Varsayılan güvenli: bir thread istemeden yetki almaz.

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
ister, `Reply` yapan istemez. Gerekçe: reply hakkı zaten istemci
tarafından verilmiş (`GRANTREPLY`), ve o hak "bana cevap ver"
demek. Cevabın içeriğini kısıtlamak ikinci bir hak biti gerektirirdi.

**Açık soruya yazıldı** — bu asimetrinin bir sömürü yolu olup
olmadığından emin değilim.

## Elenen alternatifler

**Cap hedeflerini mesajda taşımak.** Yukarıda: gönderene alıcının
CSpace'ini tarama imkânı verir.

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
- Ayarlanmamışsa hiçbir cap alınmaz. Varsayılan güvenli.
- Beş kontrol, her biri **sessizce düşürüyor**: CPtr geçersiz,
  `GRANT` yok, hedef dolu, derinlik aşıldı. Bir cap'in düşmesi
  diğerlerini etkilemiyor.
- Hata dönmemenin gerekçesi: gönderene dönmek tarama imkânı verir,
  alıcıya dönmek gereksiz — `caps` sayısı zaten söylüyor.
- Haklar transferde **değişmiyor**. Daraltma `copy` ile, önceden.
- Transfer edilen cap gönderenin **çocuğu** olur; gönderen revoke
  ederse gider. Derinlik sınırı 8 (ADR 019) burada da geçerli.
  Kapattığımız kapı: uzun delegasyon zincirleri.
- `Reply` cap taşıyabilir ve `GRANT` kontrolü sunucunun kendi
  cap'inde. **Bu asimetri açık sorulara yazıldı.**
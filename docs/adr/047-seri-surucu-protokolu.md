# ADR 047 — Seri sürücü protokolü

Status: accepted (M13)

## Bağlam

Sürücü bir bileşen. İstemcilerin ona ulaşması gerek, ve donanımın da.
İki yön, iki mekanizma:

- **İstemci → sürücü**: bir Endpoint. İstemci `Send` eder, sürücü
  `Recv` eder.
- **Donanım → sürücü**: IRQ 4'e bağlı bir Notification. Kesme geldiğinde
  kernel `signal` eder (ADR 042), sürücü uyanır.

Sürücünün ikisini birden beklemesi gerekiyor, ve bu M9c'nin **bağlı
bekleme**sinin (ADR 038) ilk gerçek kullanımı: Notification bir TCB'ye
bağlanınca, o thread Endpoint üzerinde `Recv` ederken bir sinyal
gelirse de uyanıyor.

## M12'den devreden kısıt — protokolü bu belirledi

M12'nin raporu şunu yazmıştı:

> Mesajın *kelimeleri* alıcı tarafından okunamıyor — IPC tamponu
> TCB'nin içinde ve hiçbir bileşenin adres uzayına eşlenmiş değil.
> Alıcının gördüğü şey **badge**.

Bu hâlâ böyle. Tamponu eşlemek M13'ün işi *değil* — bu milestone'un
konusu sürücü, ve tamponu eşlemek ADR 034'ün yeniden açılması demek.

Yani protokol şu kısıtla tasarlanmak zorunda: **bir mesaj yalnızca
badge taşıyabilir.** Badge 32 bit ve gönderenin seçmediği bir değer.

### İlk tasarım — ve neden yanlış olduğu

İlk tasarım yükü **badge'e** koyuyordu:
`(kimlik<<16)|(komut<<8)|bayt`.

Yazarken makul görünüyordu, uygulanırken çöktü: **badge mesaj başına
değişmiyor.** Badge capability'ye `mint` anında iliştiriliyor
(ADR 037); gönderenin her `Send`'de seçebileceği bir şey değil — ve
seçememesi onun *değeri*. Bu tasarımda bir istemcinin iki farklı
bayt göndermesi için iki ayrı capability'ye sahip olması gerekirdi.

Hata öğretici: badge'in en önemli özelliğini (gönderen seçemez) alıp
onu bir veri kanalı olarak kullanmaya çalıştım. İkisi aynı anda
doğru olamaz.

### Seçilen tasarım: kimlik badge'de, yük label'da

| alan | kim yazar | ne taşır |
|---|---|---|
| **badge** (32 bit) | root task, `mint` anında | istemci kimliği |
| **label** (RDX) | istemci, her `Send`'de | komut + bayt |

```
label:
 15            8 7             0
+---------------+---------------+
|     komut     |     bayt      |
+---------------+---------------+
```

- **bit 7:0** — yazılacak bayt.
- **bit 15:8** — komut: `WRITE` (1) veya `FINISH` (2).

Bölünme kazara değil: **istemcinin seçebildiği her şey veri,
seçemediği şey kimlik.** Sürücü label'a güvenmek zorunda değil —
bozuk bir komut yalnızca o isteği bozar — ama badge'e güvenebilir,
çünkü onu istemci yazmadı.

### Bunun gerektirdiği tek kernel değişikliği

M12'nin kısıtı: `Recv` yalnızca badge döndürüyor, label IPC
tamponunda ve tampon hiçbir bileşene eşlenmiş değil.

`Recv`'in dönüş değeri genişletiliyor: **badge alt 32 bitte, label
üst bitlerde**. İkisi de sığıyor (badge 32 bit, label 16 bit
kullanıyor) ve `ERROR_FLAG` ile çakışmıyor (bit 63).

Tamponu eşlemekten kesinlikle daha dar bir çözüm, ve öyle olması
iyi: tamponu eşlemek ADR 034'ü yeniden açmak demekti. 48 bit yük
1 KiB'lik tamponun yerini tutmuyor — ama "bir sürücü bir komut
alabiliyor" iddiası için yetiyor, ve yetmediği gün sınır görünür
olacak.

## Protokol

### İstemci tarafı

```
Endpoint.Send(ep_cptr, (CMD_WRITE << 8) | byte, 0, 0, 0)
```

`Send` **bloke eder** (ADR 033): sürücü `Recv` edene kadar istemci
bekler. Akış denetimi bedava geliyor — istemci sürücüyü boğamaz,
çünkü sürücü almadan ikinci baytı gönderemez.

### Sürücü tarafı

```
loop {
    r = Endpoint.Recv(ep)          // bloke eder; sinyal de uyandırır
    badge = r & 0xFFFF_FFFF        // kim
    label = r >> 32                // ne
    match (label >> 8) & 0xFF {
        CMD_WRITE  => IOPort.Out8(THR, label & 0xFF)
        CMD_FINISH => ...
        _          => /* kesme: label sıfır */
    }
}
```

Bir kesme geldiğinde `Recv` Notification'ın maskesiyle dönüyor
(ADR 038'in bağlı beklemesi) ve **label sıfır olur** — kesmeyi
kernel ürettiği için gönderen tarafı yok. Sürücü ikisini böyle ayırt
ediyor: sıfır komut bir istek değildir.

### Kesme yolu

1. Bir bayt gelir, UART IRQ 4'ü yükseltir.
2. Kernel: sahibi bul, hattı maskele, Notification'ı signal'le, EOI
   (ADR 042'nin dört işi).
3. Sürücü uyanır, `IOPort.In8(RBR)` ile baytı okur.
4. Sürücü `IRQHandler.Ack` ile hattı yeniden açar.

Adım 4 sürücünün işi ve kernel bunu ne zaman yapacağını bilmiyor —
ADR 042'nin kararı. Sürücü `Ack` etmezse kendi aygıtını susturmuş
olur, başkasını değil.

## Sürücünün yapılandırma yetkisi

Sürücü UART'ı **kendisi kuruyor**: DLAB'ı çevirip baud bölücüsünü
yazıyor, 8N1'i seçiyor, FIFO'yu etkinleştiriyor, MCR'de OUT2'yi set
ediyor (IBM PC'de kesme hattını veriyoluna bu bağlıyor), ve IER'de
"received data available" kesmesini açıyor.

Bu sekiz yazmacın hepsine dokunmak demek, ve tam olarak bu yüzden
sürücü *sürücü*: aygıtın yapılandırması onun.

Kernel'in acil konsolu bu yazmaçların hiçbirine dokunmuyor (ADR 046,
durum 2) — tek istisnası panic yolu, ve o istisnanın koşulu "sistem
zaten bitti".

## Elenen alternatifler

**Paylaşılan tampon Frame + Notification.** Doğru cevap, ve bir gün
gerekli. Şimdi değil: ikinci bir mekanizma, ikinci bir yaşam döngüsü
sorusu (tamponu kim sahiplenir, sürücü ölünce ne olur), ve
milestone'un iddiası için gereksiz.

**Sürücünün istemciye `Reply` ile cevap vermesi.** M9b'nin call/reply'i
var ve kullanılabilirdi. Kullanılmadı çünkü `WRITE` için cevaba gerek
yok — `Send`'in dönmesi zaten "sürücü aldı" demek, rendezvous
semantiği bunu garanti ediyor (ADR 033). Gereksiz bir round-trip
eklemek, bedavaymış gibi görünen ama iki kat bağlam değişimi
maliyetinde bir şey.

**Sürücü başına bir Endpoint yerine istemci başına bir Endpoint.**
Badge'in var olma sebebini ortadan kaldırırdı: tek Endpoint + badge,
sunucunun istemcileri *güvenmeden* ayırt etmesinin yolu (ADR 037).

## Sonuç

- Tek Endpoint, badge ile çoklanmış.
- Badge = `(client_id << 16) | (cmd << 8) | byte`; kimlik alanını root
  task atıyor, gönderen seçemiyor.
- IRQ 4 → Notification → sürücünün TCB'sine bağlı (ADR 038).
- `Ack` sürücünün sorumluluğu (ADR 042).
- Mesaj başına bir bayt — yazılı ödün, tampon eşleme gerektirdiği için
  ertelendi.
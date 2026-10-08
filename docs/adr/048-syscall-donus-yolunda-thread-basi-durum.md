# ADR 048 — Syscall dönüş yolunda thread başına durum

- **Durum:** Kabul edildi
- **Tarih:** M13
- **Bağlam:** ADR 029 (sysret/iretq), ADR 030 (syscall ABI), ADR 033 (IPC randevu)

## Bağlam

M13'te `driver-dies` senaryosu, root task'ın sürücüsünü öldürmeden önce
onun *gerçekten çalıştığını* beklemesini gerektirdi. Beklemenin doğru
yolu `Send`'di: randevu olduğu için, alıcı mesajı alana kadar dönmez
(ADR 033).

Root task o `Send`'den döndü ve **kendi yığınının bir sayfa ötesinde**
bir adresi okuyarak #PF aldı:

```
rip=0x401fe2  mov 0x90(%rsp),%rdi
rsp=0x500f88          ; kendi yığını 0x500000..0x501000, başlangıç 0x500ff0
cr2=0x501018          ; = 0x500f88 + 0x90
```

Okuduğu ofset doğruydu; `rsp`'si yanlıştı. Dönerken **seri port
sürücüsünün yığın işaretçisiyle** devam etmişti.

## Hata

Giriş stub'ı (ADR 030) kullanıcının `RSP`'sini tek bir global statik
üzerinden taşıyordu:

```asm
syscall_entry:
    mov [rip + SAVED_USER_RSP], rsp     ; 1. parka al
    mov rsp, [rip + KERNEL_RSP]         ; 2. çekirdek yığınına geç
    push qword ptr [rip + SAVED_USER_RSP]  ; 3. frame'e yaz
    ...
    call syscall_dispatch
    ...
    mov rsp, [rip + SAVED_USER_RSP]     ; HATA: statiği tekrar oku
    sysretq
```

1-3 arası güvenlidir: IF temizdir, araya hiçbir şey giremez. Statiğin
ömrü o iki komuttur ve orada bir **devir teslimdir**, depolama değil.

Son satır ise statiği `syscall_dispatch` *döndükten sonra* okur. Syscall
bloke olmuşsa — `Send` alıcı beklerken, `Recv` gönderici beklerken —
dönüş bir bağlam anahtarının öbür yakasında gerçekleşir. Arada başka
thread'ler çekirdeğe girmiş ve statiği kendi yığın işaretçileriyle
ezmiştir. Dönen thread, **en son syscall yapan thread'in yığınına**
`sysret` eder.

Doğru değer zaten frame'in içindeydi: 3. adım onu oraya yazmıştı ve
frame bu thread'in çekirdek yığınında durur — zamanlayıcının kaydedip
geri yüklediği yerde.

## Neden M9'dan M13'e kadar görünmedi

Hata, bloke eden IPC'nin eklendiği M9'dan beri oradaydı. 59 senaryo
geçti çünkü bloke olan her program **elle yazılmış assembly**'ydi:
`Send`, ardından `Echo`, ardından `jmp $`. Hiçbiri yığına dokunmuyordu.
Yanlış bir `RSP` ile `sysret` etmek, o `RSP` hiç dereference edilmediği
sürece gözlenebilir bir şey yapmaz.

Root task, bloke eden bir `Send`'den dönüp **yerel değişkenleri olan
koda** devam eden ilk derlenmiş bileşendir. Hatayı M13 üretmedi; M13
hatanın gözlenebilir hale geldiği ilk yerdir.

Bu, kabul testlerinin ne kadar ileri gidebileceğinin ölçüsüdür: 59
senaryonun hepsi geçerken, hiçbiri dönüş yolundaki `RSP`'yi *kullanan*
bir program çalıştırmıyordu. Test etmediğiniz şey, test ettiğiniz
şeyin yanında sessizce yanlış olabilir.

## Karar

**Dönüş yolu kullanıcı durumunu yalnızca frame'den okur.**

`sysret` yolunda, on beş `pop`'tan sonra `RSP` zaten frame'in
`user_rsp` alanını gösterir:

```asm
    mov rsp, [rsp]
    sysretq
```

`iretq` yolunda araya bir `push` girdiği için `[rsp + 8]`.

`SAVED_USER_RSP` yalnızca giriş stub'ının ilk iki komutu arasında
yaşar. Bu, bir *pencere*dir ve penceresi IF ile kapalıdır.

## Değerlendirilen alternatifler

**(a) Statiği thread başına yapmak** — GS tabanlı per-CPU alan ya da
TCB'de bir slot. Çalışır, ama yanlış soruyu çözer: değer zaten frame'de
*var*. İkinci bir kopya tutmak, iki kopyanın uyuşmasını yeni bir
değişmez haline getirir.

**(b) `syscall_dispatch`'in frame'i düzeltmesi** — bloke eden yollarda
`frame.user_rsp`'yi yeniden yazmak. Bu, her bloke eden metodun
hatırlaması gereken bir adım ekler; unutulabilen temizlik çağrısının
tanımı.

**(c) Frame'den okumak** — seçildi. Fazladan durum yok, hatırlanacak
adım yok. Doğru değerin zaten bulunduğu yerden okunur.

## Sonuçlar

- `sysret` yolu bir bellek erişimi kazanmaz: zaten statiği okuyordu,
  şimdi frame'i okuyor. Aynı maliyet.
- Statiğin ömrü artık iki komut. Bir sonraki okuyan, onu kesintisiz
  bir pencerede kullanmak zorunda olduğunu stub'ın başındaki notta
  görür.
- Aynı şekil `KERNEL_RSP` için de geçerlidir ve orada **sorun
  değildir**: giriş stub'ı onu 2. adımda okur, yani aynı kapalı
  pencerenin içinde. Fark, okumanın `syscall_dispatch`'ten önce mi
  sonra mı olduğudur — ve bu ayrım genel bir kuraldır: **çekirdeğe
  girişte yakalanan hiçbir kullanıcı durumu, dispatch'ten sonra
  global bir yerden okunamaz.**

## Doğrulama

`driver-dies` ve `driver-restart`, düzeltmeden önce #PF ile düşüyordu;
sonrasında ikisi de 33 ile çıkıyor. Diğer 59 senaryo etkilenmez —
zaten hiçbiri bu yolu gözlemleyemiyordu, ki ADR'nin varlık sebebi de
budur.
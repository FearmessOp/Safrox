# ADR 026 — TCB'nin yaşam döngüsü: çalışan bir thread'i öldürmek

Status: accepted (M6)

Soru dar ama cevabı kaçınılmaz: **çalışmakta olan bir thread'in TCB
capability'si revoke edilirse ne olur?**

Master prompt'un 4. kuralı ("crash-only") bunu zorunlu kılıyor: iki
cevap da savunulabilir, "tanımsız" savunulamaz.

## Üç olasılık

**(a) Reddet.** `revoke`, hedef TCB çalışıyorsa veya hazır kuyruğundaysa
`ThreadBusy` döndürür. Önce `Suspend`, sonra revoke.

**(b) Zorla öldür.** Thread kuyruktan çıkarılır, çalışıyorsa bağlam
değiştirme zorlanır, kaynakları Untyped'a döner.

**(c) Ertele.** Revoke kaydedilir, thread bir sonraki yield'de ölür.

## Karar: (a) reddet

`revoke` ve `delete`, hedefi çalışan veya çalışmaya hazır bir thread
olan bir TCB capability'si üzerinde **`ThreadBusy` döndürür.**

Sıra şu:

```
Suspend(tcb)    -> thread kuyruktan çıkar, durumu Suspended olur
revoke(untyped) -> artık serbest
```

### Neden (b) değil

Zorla öldürmek, "çalışıyorsa" durumunu çözmek zorunda. Thread **şu
anda CPU'da** ise, onu öldüren kod o thread'in kendi stack'inde
çalışıyor olabilir — kendi altındaki zemini çekmek.

Somut: thread A, thread B'nin TCB'sini revoke ediyor. B'nin kernel
stack'i serbest bırakılıyor. Ama B, bir syscall'ın ortasında bloke
olmuş olabilir ve o stack'te yarım kalmış bir kernel çerçevesi var. B
asla dönmeyecek, ama stack'i Untyped'a döndüğünde o çerçeve başka bir
nesnenin altında kalıyor.

Bu çözülebilir — seL4 çözüyor, "zombie" durumu ve kademeli yıkımla. Ama
çözümü M6'nın kapsamından büyük ve M7'nin (henüz yazılmamış) IPC
bloklanma semantiğine bağlı: bir Endpoint'te bekleyen thread'in
öldürülmesi, o Endpoint'in kuyruğundan çıkarılmasını gerektirir.

### Neden (c) değil

Ertelenmiş revoke, revoke'un anlamını bozar. ADR 019'un gerekçesi
aynen geçerli: "belki geri alır" bir revoke değildir. Çağıran döndüğünde
yetki ya gitmiştir ya gitmemiştir; "birazdan gidecek" üçüncü bir durum
ve kimse onunla ne yapacağını bilemez.

### (a)'nın maliyeti: dürüst kabul

Reddetmek sorunu çözmüyor, **çağırana devrediyor.** Ve devrettiği şey
şu: bir bileşen, kendisini öldürmeye çalışan başka bir bileşeni,
sürekli çalışır durumda kalarak engelleyebilir mi?

Hayır, ve nedeni önemli: `Suspend` çalışan bir thread üzerinde
**çalışır.** Reddedilen şey yalnızca revoke; suspend her zaman mümkün.
Yani öldürme dizisi iki adım ama ikinci adım garantili:

```
Suspend(tcb)     -> her zaman başarılı
revoke(untyped)  -> suspend'den sonra her zaman başarılı
```

Bir bileşen kendi TCB'sini suspend edemez mi? Edebilir — ve o zaman
kendini durdurmuş olur, ki bu zaten bir seçenek.

## "Çalışıyor" ne demek: durumlar

```
         Resume          timer / yield
Suspended ----> Ready <---------------> Running
    ^             |                        |
    |   Suspend   |         Suspend        |
    +-------------+------------------------+
```

| Durum | Anlamı | revoke edilebilir mi |
|---|---|---|
| `Inactive` | Retype edilmiş ama hiç başlatılmamış | **evet** |
| `Ready` | Hazır kuyruğunda | hayır — `ThreadBusy` |
| `Running` | CPU'da | hayır — `ThreadBusy` |
| `Suspended` | Kuyruktan çıkarılmış | **evet** |

`Inactive` ve `Suspended` arasındaki fark yalnızca bağlamın anlamlı
olup olmadığı; revoke açısından ikisi de serbest.

M9'da `Blocked` eklenecek (bir Endpoint'te bekliyor) ve o durumun
revoke'u, Endpoint'in kuyruğundan çıkarmayı gerektirecek — bu ADR'nin
M9'da yeniden ele alınması gereken yeri.

## Kernel stack'inin sahipliği

Her TCB'nin kendi kernel stack'i var. Stack, TCB'nin parçası **değil**
(2048 bayta 16 KiB sığmaz); ayrı bir bellek bölgesi, ve sahipliği
TCB'nin kendi alanında kayıtlı.

Bu, M6'da bir soruyu doğuruyor: stack'in çerçeveleri nereden geliyor?
Çerçeve ayırıcı boot'un sonunda emekli oluyor (ADR 022), yani
thread oluşturma bir Untyped'dan beslenmeli.

**Karar: stack'ler de retype ile geliyor.** `TcbSetStack` invocation'ı
bir Frame capability'si alıyor ve onu TCB'nin stack'i olarak
bağlıyor — yani stack'in bütçesi, onu oluşturan bileşenin bütçesi.
Kernel kendi adına stack ayırmıyor.

M6'nın testi bunu yapıyor: Untyped'dan 4 Frame retype ediyor, KSTACK
aralığına haritalıyor, TCB'ye bağlıyor.

Stack'in **haritalanması** kernel'in işi (kernel adres alanında,
guard page ile), ama **çerçeveleri** çağıranın. Ara sayfa tabloları
için gereken çerçeveler de aynı Frame'lerden gelmeli, yoksa ADR 022'nin
heap'te düştüğü tuzağın aynısına düşeriz: veriyi ödeyip yapıyı
ödememek.

## Elenen alternatif

**TCB'yi hiç revoke edilemez yapmak.** "Thread'ler sonsuza kadar
yaşar." M13'ün init sistemi bileşen yeniden başlatacak; kaynak geri
alınamazsa her yeniden başlatma sızdırır.

## Sonuç

- Çalışan veya hazır bir thread'in TCB'si **revoke edilemez**:
  `ThreadBusy`.
- Öldürme dizisi iki adım: `Suspend` (her zaman başarılı), sonra
  revoke. İkinci adımın garantili olması, reddetmeyi bir engelleme
  vektörü olmaktan çıkarıyor.
- Durumlar: `Inactive`, `Ready`, `Running`, `Suspended`. İlk ve son
  revoke edilebilir.
- **M9'da yeniden ele alınacak:** `Blocked` durumu, Endpoint
  kuyruğundan çıkarma gerektiriyor.
- Kernel stack'leri Untyped'dan retype edilen Frame'lerden; kernel
  thread için kendi adına bellek ayırmıyor. Ara tablolar da aynı
  kaynaktan (ADR 022'nin heap'te öğrendiği ders).
/* SPDX-License-Identifier: MPL-2.0 */
/*
 * M15 spike: does QMP `send-key` actually deliver a scan code to
 * port 0x60 under `-M q35 -display none`?
 *
 * Throwaway. Deleted once the question is answered.
 *
 * Why a boot sector rather than a Safrox scenario: the question is
 * about QEMU, not about Safrox. A spike that boots the kernel would
 * answer "does the whole stack work", and if it printed nothing we
 * could not tell whether the scan code never arrived or whether the
 * capability plumbing dropped it. Sixty lines of real mode depend on
 * nothing at all, so a silent result here means QEMU did not deliver
 * the byte -- which is exactly the fact in question.
 *
 * What it does: poll the i8042 status register (0x64) for bit 0
 * (output buffer full), read the byte from 0x60, print it to COM1 as
 * two hex digits, repeat. No controller initialisation, because the
 * BIOS has already done it -- and because whether initialisation is
 * needed is itself one of M15's open questions.
 */

	.code16
	.section .text
	.global _start

_start:
	cli
	xor	%ax, %ax
	mov	%ax, %ds
	mov	%ax, %es
	mov	%ax, %ss
	mov	$0x7c00, %sp

	/* COM1: 115200 8N1. The divisor is 1, which is what the
	 * project's own serial code uses (ADR: 115200 is QEMU's
	 * fastest and costs nothing in emulation). */
	mov	$0x3fb, %dx		/* LCR */
	mov	$0x80, %al		/* DLAB on */
	out	%al, %dx
	mov	$0x3f8, %dx		/* divisor low */
	mov	$0x01, %al
	out	%al, %dx
	mov	$0x3f9, %dx		/* divisor high */
	xor	%al, %al
	out	%al, %dx
	mov	$0x3fb, %dx		/* LCR: 8N1, DLAB off */
	mov	$0x03, %al
	out	%al, %dx

	/* Announce. If this line is missing, the spike itself is
	 * broken and the keyboard result means nothing. */
	mov	$banner, %si
	call	puts

poll:
	mov	$0x64, %dx
	in	%dx, %al
	test	$0x01, %al		/* output buffer full? */
	jz	poll

	mov	$0x60, %dx
	in	%dx, %al		/* the scan code */
	mov	%al, %bl
	call	puthex
	mov	$' ', %al
	call	putc
	jmp	poll

/* Print BL as two hex digits. */
puthex:
	mov	%bl, %al
	shr	$4, %al
	call	putnib
	mov	%bl, %al
	and	$0x0f, %al
	call	putnib
	ret

putnib:
	cmp	$10, %al
	jb	1f
	add	$('a' - 10), %al
	jmp	putc
1:	add	$'0', %al
	/* fall through */

/* Print AL on COM1, waiting for the transmit holding register. */
putc:
	push	%ax
	push	%dx
2:	mov	$0x3fd, %dx		/* LSR */
	in	%dx, %al
	test	$0x20, %al		/* THR empty? */
	jz	2b
	pop	%dx
	pop	%ax
	push	%dx
	mov	$0x3f8, %dx
	out	%al, %dx
	pop	%dx
	ret

/* Print the NUL-terminated string at SI. */
puts:
	lodsb
	test	%al, %al
	jz	3f
	call	putc
	jmp	puts
3:	ret

banner:
	.asciz	"SPIKE-READY "

	.org	510
	.word	0xaa55
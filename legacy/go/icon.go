//go:build windows

package main

import (
	"errors"
	"image"
	"image/color"
	"math"
	"unsafe"
)

// Icon look: a Tux silhouette filling the icon, coloured by state.
//
// Load levels (the higher of CPU and memory share of the host):
//
//	green  < 50 %, orange 50-75 %, red > 75 %; gray = WSL2 is off.
type level int

const (
	levelOff level = iota
	levelOK
	levelWarn
	levelHigh
)

var levelColors = map[level][3]byte{ // R,G,B
	levelOff:  {150, 150, 150},
	levelOK:   {52, 199, 89},
	levelWarn: {255, 149, 0},
	levelHigh: {255, 59, 48},
}

const hollowAlpha = 110 // alpha (0-255) of Tux's belly/face areas

// levelFor classifies a status. Unknown CPU (-1) counts as 0.
func levelFor(st Status) level {
	if !st.Running {
		return levelOff
	}
	load := max(st.CPU, st.MemPct)
	switch {
	case load > 75:
		return levelHigh
	case load >= 50:
		return levelWarn
	}
	return levelOK
}

// renderIcon builds a premultiplied 32-bpp ARGB image and turns it into an
// HICON. If keepPixels is set, a copy of the pixels (BGRA, premultiplied) is
// returned too, which the -render-test mode uses to write PNGs.
func renderIcon(sz int, lv level, keepPixels bool) (uintptr, []byte, error) {
	hdcScreen, _, _ := pGetDC.Call(0)
	defer pReleaseDC.Call(0, hdcScreen)
	hdc, _, _ := pCreateCompatibleDC.Call(hdcScreen)
	if hdc == 0 {
		return 0, nil, errors.New("CreateCompatibleDC failed")
	}
	defer pDeleteDC.Call(hdc)

	bmi := bitmapInfo{Header: bitmapInfoHeader{
		Size: uint32(unsafe.Sizeof(bitmapInfoHeader{})), Width: int32(sz), Height: -int32(sz),
		Planes: 1, BitCount: 32, Compression: biRGB,
	}}
	var bits unsafe.Pointer
	hbm, _, _ := pCreateDIBSection.Call(hdc, uintptr(unsafe.Pointer(&bmi)), dibRGBColors, uintptr(unsafe.Pointer(&bits)), 0, 0)
	if hbm == 0 || bits == nil {
		return 0, nil, errors.New("CreateDIBSection failed")
	}
	defer pDeleteObject.Call(hbm)
	pix := unsafe.Slice((*byte)(bits), sz*sz*4)
	drawIcon(pix, sz, lv)

	var copyPix []byte
	if keepPixels {
		copyPix = append([]byte(nil), pix...)
	}

	mask, _, _ := pCreateBitmap.Call(uintptr(sz), uintptr(sz), 1, 1, 0)
	if mask == 0 {
		return 0, nil, errors.New("CreateBitmap failed")
	}
	defer pDeleteObject.Call(mask)
	ii := iconInfo{FIcon: 1, HbmMask: mask, HbmColor: hbm}
	hicon, _, _ := pCreateIconIndirect.Call(uintptr(unsafe.Pointer(&ii)))
	if hicon == 0 {
		return 0, nil, errors.New("CreateIconIndirect failed")
	}
	return hicon, copyPix, nil
}

// drawIcon paints the icon into pix (BGRA premultiplied, sz*sz).
func drawIcon(pix []byte, sz int, lv level) {
	clear(pix)
	c := levelColors[lv]
	put := func(x, y int, a uint32) {
		i := (y*sz + x) * 4
		pix[i+0] = byte(uint32(c[2]) * a / 255)
		pix[i+1] = byte(uint32(c[1]) * a / 255)
		pix[i+2] = byte(uint32(c[0]) * a / 255)
		pix[i+3] = byte(a)
	}

	// Fit the glyph into the square, keeping its aspect ratio, centred.
	glyphH := sz
	glyphW := int(math.Round(float64(glyphH) * tuxMaskW / tuxMaskH))
	if glyphW > sz {
		glyphW = sz
		glyphH = int(math.Round(float64(glyphW) * tuxMaskH / tuxMaskW))
	}
	cov := resample(tuxMask, tuxMaskW, tuxMaskH, glyphW, glyphH)
	holes := resample(tuxHoles, tuxMaskW, tuxMaskH, glyphW, glyphH)
	x0, y0 := (sz-glyphW)/2, (sz-glyphH)/2
	for y := 0; y < glyphH; y++ {
		for x := 0; x < glyphW; x++ {
			i := y*glyphW + x
			// Belly/face are tinted at low alpha so the silhouette reads as a
			// solid shape instead of a thin outline on dark taskbars.
			a := max(uint32(cov[i]), uint32(holes[i])*hollowAlpha/255)
			if a > 0 {
				put(x0+x, y0+y, a)
			}
		}
	}
}

// resample shrinks a grayscale coverage mask with area averaging, which gives
// clean antialiased edges at tray sizes.
func resample(src []byte, sw, sh, dw, dh int) []byte {
	dst := make([]byte, dw*dh)
	fx := float64(sw) / float64(dw)
	fy := float64(sh) / float64(dh)
	for y := 0; y < dh; y++ {
		sy0, sy1 := float64(y)*fy, float64(y+1)*fy
		for x := 0; x < dw; x++ {
			sx0, sx1 := float64(x)*fx, float64(x+1)*fx
			var sum, area float64
			for sy := int(sy0); sy < int(math.Ceil(sy1)) && sy < sh; sy++ {
				wy := math.Min(sy1, float64(sy+1)) - math.Max(sy0, float64(sy))
				for sx := int(sx0); sx < int(math.Ceil(sx1)) && sx < sw; sx++ {
					wx := math.Min(sx1, float64(sx+1)) - math.Max(sx0, float64(sx))
					sum += float64(src[sy*sw+sx]) * wx * wy
					area += wx * wy
				}
			}
			if area > 0 {
				dst[y*dw+x] = byte(math.Round(sum / area))
			}
		}
	}
	return dst
}

// pixelsToImage converts premultiplied BGRA to a straight-alpha NRGBA image.
func pixelsToImage(sz int, pix []byte) *image.NRGBA {
	img := image.NewNRGBA(image.Rect(0, 0, sz, sz))
	for y := 0; y < sz; y++ {
		for x := 0; x < sz; x++ {
			i := (y*sz + x) * 4
			a := uint32(pix[i+3])
			var r, g, b uint32
			if a > 0 {
				b = uint32(pix[i+0]) * 255 / a
				g = uint32(pix[i+1]) * 255 / a
				r = uint32(pix[i+2]) * 255 / a
			}
			img.SetNRGBA(x, y, color.NRGBA{R: byte(r), G: byte(g), B: byte(b), A: byte(a)})
		}
	}
	return img
}

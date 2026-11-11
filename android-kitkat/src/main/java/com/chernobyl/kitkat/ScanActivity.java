package com.chernobyl.kitkat;

import android.app.Activity;
import android.content.Intent;
import android.hardware.Camera;
import android.os.Bundle;
import android.view.Gravity;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.widget.FrameLayout;
import android.widget.TextView;
import com.google.zxing.BarcodeFormat;
import com.google.zxing.BinaryBitmap;
import com.google.zxing.DecodeHintType;
import com.google.zxing.PlanarYUVLuminanceSource;
import com.google.zxing.Result;
import com.google.zxing.common.HybridBinarizer;
import com.google.zxing.qrcode.QRCodeReader;
import java.util.Collections;
import java.util.EnumMap;
import java.util.Map;

/**
 * Scans the companion's pairing QR with the KitKat camera API
 * (android.hardware.Camera preview frames) and ZXing core. Returns the decoded
 * text; the Pair screen validates it like a pasted line.
 */
@SuppressWarnings("deprecation")
public final class ScanActivity extends Activity implements SurfaceHolder.Callback, Camera.PreviewCallback {
    private Camera camera;
    private final QRCodeReader reader = new QRCodeReader();
    private final Map<DecodeHintType, Object> hints = new EnumMap<DecodeHintType, Object>(DecodeHintType.class);
    private volatile boolean decoding;
    private TextView label;

    @Override
    protected void onCreate(Bundle b) {
        super.onCreate(b);
        hints.put(DecodeHintType.POSSIBLE_FORMATS, Collections.singletonList(BarcodeFormat.QR_CODE));
        hints.put(DecodeHintType.TRY_HARDER, Boolean.TRUE);
        FrameLayout root = new FrameLayout(this);
        root.setBackgroundColor(Ui.BLACK);
        SurfaceView sv = new SurfaceView(this);
        sv.getHolder().addCallback(this);
        sv.getHolder().setType(SurfaceHolder.SURFACE_TYPE_PUSH_BUFFERS);
        root.addView(sv);
        label = Ui.text(this, "Point the camera at the pairing QR on your PC", 16, Ui.WHITE, true);
        label.setGravity(Gravity.CENTER);
        label.setPadding(0, Ui.dp(this, 24), 0, Ui.dp(this, 24));
        label.setBackgroundColor(0x99000000);
        root.addView(label, new FrameLayout.LayoutParams(FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.WRAP_CONTENT, Gravity.BOTTOM));
        setContentView(root);
    }

    @Override
    public void surfaceCreated(SurfaceHolder h) {
        try {
            camera = Camera.open();
            if (camera == null && Camera.getNumberOfCameras() > 0) camera = Camera.open(0); // front-only tablets
            if (camera == null) throw new RuntimeException("no camera");
            Camera.Parameters p = camera.getParameters();
            if (p.getSupportedFocusModes().contains(Camera.Parameters.FOCUS_MODE_CONTINUOUS_PICTURE))
                p.setFocusMode(Camera.Parameters.FOCUS_MODE_CONTINUOUS_PICTURE);
            camera.setParameters(p);
            camera.setDisplayOrientation(getResources().getConfiguration().orientation == android.content.res.Configuration.ORIENTATION_PORTRAIT ? 90 : 0);
            camera.setPreviewDisplay(h);
            camera.setPreviewCallback(this);
            camera.startPreview();
        } catch (Exception e) {
            label.setText("Camera unavailable: paste the payload line instead");
        }
    }

    @Override
    public void surfaceChanged(SurfaceHolder h, int f, int w, int ht) { }

    @Override
    public void surfaceDestroyed(SurfaceHolder h) {
        release();
    }

    @Override
    protected void onPause() {
        super.onPause();
        release();
        finish();
    }

    private void release() {
        if (camera == null) return;
        camera.setPreviewCallback(null);
        camera.stopPreview();
        camera.release();
        camera = null;
    }

    @Override
    public void onPreviewFrame(final byte[] data, Camera cam) {
        if (decoding) return;
        decoding = true;
        Camera.Size s = cam.getParameters().getPreviewSize();
        final int w = s.width, h = s.height;
        new Thread(new Runnable() {
            @Override
            public void run() {
                try {
                    PlanarYUVLuminanceSource src = new PlanarYUVLuminanceSource(data, w, h, 0, 0, w, h, false);
                    final Result r = reader.decode(new BinaryBitmap(new HybridBinarizer(src)), hints);
                    runOnUiThread(new Runnable() {
                        @Override
                        public void run() {
                            setResult(RESULT_OK, new Intent().putExtra("text", r.getText()));
                            finish();
                        }
                    });
                } catch (Exception notFound) {
                    decoding = false;
                }
            }
        }).start();
    }
}

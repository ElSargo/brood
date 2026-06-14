#include <GL/glut.h>
#include <stdio.h>

static float angle = 0.0f;

static void draw_face(float r, float g, float b, float nx, float ny, float nz) {
    glColor3f(r, g, b);
    glNormal3f(nx, ny, nz);
    glBegin(GL_QUADS);
    if (nz > 0.0f) {
        glVertex3f(-1.0f, -1.0f, 1.0f);
        glVertex3f(1.0f, -1.0f, 1.0f);
        glVertex3f(1.0f, 1.0f, 1.0f);
        glVertex3f(-1.0f, 1.0f, 1.0f);
    } else if (nz < 0.0f) {
        glVertex3f(1.0f, -1.0f, -1.0f);
        glVertex3f(-1.0f, -1.0f, -1.0f);
        glVertex3f(-1.0f, 1.0f, -1.0f);
        glVertex3f(1.0f, 1.0f, -1.0f);
    } else if (nx > 0.0f) {
        glVertex3f(1.0f, -1.0f, 1.0f);
        glVertex3f(1.0f, -1.0f, -1.0f);
        glVertex3f(1.0f, 1.0f, -1.0f);
        glVertex3f(1.0f, 1.0f, 1.0f);
    } else if (nx < 0.0f) {
        glVertex3f(-1.0f, -1.0f, -1.0f);
        glVertex3f(-1.0f, -1.0f, 1.0f);
        glVertex3f(-1.0f, 1.0f, 1.0f);
        glVertex3f(-1.0f, 1.0f, -1.0f);
    } else if (ny > 0.0f) {
        glVertex3f(-1.0f, 1.0f, 1.0f);
        glVertex3f(1.0f, 1.0f, 1.0f);
        glVertex3f(1.0f, 1.0f, -1.0f);
        glVertex3f(-1.0f, 1.0f, -1.0f);
    } else {
        glVertex3f(-1.0f, -1.0f, -1.0f);
        glVertex3f(1.0f, -1.0f, -1.0f);
        glVertex3f(1.0f, -1.0f, 1.0f);
        glVertex3f(-1.0f, -1.0f, 1.0f);
    }
    glEnd();
}

static void display(void) {
    glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
    glLoadIdentity();
    glTranslatef(0.0f, 0.0f, -5.0f);
    glRotatef(angle, 1.0f, 1.0f, 0.2f);

    draw_face(0.95f, 0.25f, 0.20f, 0.0f, 0.0f, 1.0f);
    draw_face(0.10f, 0.55f, 0.95f, 0.0f, 0.0f, -1.0f);
    draw_face(0.20f, 0.80f, 0.35f, 1.0f, 0.0f, 0.0f);
    draw_face(0.95f, 0.80f, 0.20f, -1.0f, 0.0f, 0.0f);
    draw_face(0.70f, 0.35f, 0.95f, 0.0f, 1.0f, 0.0f);
    draw_face(0.95f, 0.45f, 0.10f, 0.0f, -1.0f, 0.0f);

    glutSwapBuffers();
}

static void reshape(int width, int height) {
    if (height == 0) {
        height = 1;
    }
    glViewport(0, 0, width, height);
    glMatrixMode(GL_PROJECTION);
    glLoadIdentity();
    gluPerspective(55.0, (double)width / (double)height, 0.1, 100.0);
    glMatrixMode(GL_MODELVIEW);
}

static void tick(int value) {
    (void)value;
    angle += 1.0f;
    if (angle >= 360.0f) {
        angle -= 360.0f;
    }
    glutPostRedisplay();
    glutTimerFunc(16, tick, 0);
}

int main(int argc, char **argv) {
    glutInit(&argc, argv);
    glutInitDisplayMode(GLUT_DOUBLE | GLUT_RGB | GLUT_DEPTH);
    glutInitWindowSize(640, 480);
    glutCreateWindow("WSLg accelerated OpenGL cube");

    printf("GL_VENDOR=%s\n", glGetString(GL_VENDOR));
    printf("GL_RENDERER=%s\n", glGetString(GL_RENDERER));
    printf("GL_VERSION=%s\n", glGetString(GL_VERSION));
    fflush(stdout);

    glEnable(GL_DEPTH_TEST);
    glEnable(GL_CULL_FACE);
    glClearColor(0.05f, 0.06f, 0.08f, 1.0f);

    glutDisplayFunc(display);
    glutReshapeFunc(reshape);
    glutTimerFunc(16, tick, 0);
    glutMainLoop();
    return 0;
}
